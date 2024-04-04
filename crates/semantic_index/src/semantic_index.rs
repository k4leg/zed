use anyhow::{anyhow, Context as _, Result};
use collections::HashMap;
use fs::Fs;
use futures::{
    channel::mpsc::{self, Receiver, Sender},
    SinkExt, StreamExt,
};
use futures_batch::ChunksTimeoutStreamExt;
use gpui::{AppContext, Context, EventEmitter, Global, Model, ModelContext, Task, WeakModel};
use language::{LanguageRegistry, Tree, PARSER};
use project::{Entry, Project, Worktree};
use sha2::{Digest, Sha256};
use smol::channel;
use std::{cmp, ops::Range, path::Path, sync::Arc, time::Duration};
use util::ResultExt;

pub struct SemanticIndex {
    db: lmdb::Database,
    project_indices: HashMap<WeakModel<Project>, Model<ProjectIndex>>,
}

impl Global for SemanticIndex {}

impl SemanticIndex {
    pub fn new(db_path: &Path) -> Result<Self> {
        dbg!(&db_path);
        let env = lmdb::Environment::new()
            .open(db_path)
            .context("failed to open environment")?;
        let db = env
            .create_db(None, lmdb::DatabaseFlags::empty())
            .context("failed to create db")?;

        Ok(SemanticIndex {
            db,
            project_indices: HashMap::default(),
        })
    }

    pub fn project_index(
        &mut self,
        project: Model<Project>,
        cx: &mut AppContext,
    ) -> Model<ProjectIndex> {
        self.project_indices
            .entry(project.downgrade())
            .or_insert_with(|| cx.new_model(|cx| ProjectIndex::new(project, self.db.clone(), cx)))
            .clone()
    }
}

pub struct ProjectIndex {
    db: lmdb::Database,
    project: WeakModel<Project>,
    worktree_scans: HashMap<WeakModel<Worktree>, Task<()>>,
    language_registry: Arc<LanguageRegistry>,
    fs: Arc<dyn Fs>,
}

impl ProjectIndex {
    fn new(project: Model<Project>, db: lmdb::Database, cx: &mut ModelContext<Self>) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let fs = project.read(cx).fs().clone();
        let mut this = ProjectIndex {
            db,
            project: project.downgrade(),
            worktree_scans: HashMap::default(),
            language_registry,
            fs,
        };

        for worktree in project.read(cx).worktrees().collect::<Vec<_>>() {
            this.add_worktree(worktree, cx);
        }

        this
    }

    fn add_worktree(&mut self, worktree: Model<Worktree>, cx: &mut ModelContext<Self>) {
        let Some(local_worktree) = worktree.read(cx).as_local() else {
            return;
        };
        let local_worktree = local_worktree.snapshot();

        if self.worktree_scans.is_empty() {
            cx.emit(StatusEvent::Scanning);
        }

        let language_registry = self.language_registry.clone();
        let fs = self.fs.clone();

        let (entries_tx, entries_rx) = channel::bounded(512);
        let worktree_abs_path = local_worktree.abs_path().clone();
        let scan_entries = cx.background_executor().spawn(async move {
            for entry in local_worktree.files(false, 0) {
                if entries_tx.send(entry.clone()).await.is_err() {
                    break;
                }
            }
        });

        let (chunked_files_tx, chunked_files_rx) = mpsc::channel(2048);
        let embed_chunks = embed_chunks(chunked_files_rx, cx);

        let worktree = worktree.downgrade();
        let scan = cx.spawn(|this, mut cx| {
            let worktree = worktree.clone();
            async move {
                cx.background_executor()
                    .scoped(|cx| {
                        for _ in 0..cx.num_cpus() {
                            cx.spawn(async {
                                while let Ok(entry) = entries_rx.recv().await {
                                    index_entry(
                                        worktree_abs_path.clone(),
                                        entry,
                                        &language_registry,
                                        fs.as_ref(),
                                        chunked_files_tx.clone(),
                                    )
                                    .await
                                    .log_err();
                                }
                            });
                        }
                    })
                    .await;
                scan_entries.await;
                embed_chunks.await;

                this.update(&mut cx, |this, cx| {
                    this.worktree_scans.remove(&worktree);
                    if this.worktree_scans.is_empty() {
                        cx.emit(StatusEvent::Idle);
                    }
                })
                .ok();
            }
        });
        self.worktree_scans.insert(worktree, scan);
    }
}

impl EventEmitter<StatusEvent> for ProjectIndex {}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StatusEvent {
    Idle,
    Scanning,
}

struct ChunkedFile {
    worktree_root: Arc<Path>,
    entry: Entry,
    text: String,
    chunks: Vec<Chunk>,
}

struct Chunk {
    range: Range<usize>,
    digest: [u8; 32],
}

async fn index_entry(
    worktree_root: Arc<Path>,
    entry: Entry,
    language_registry: &Arc<LanguageRegistry>,
    fs: &dyn Fs,
    mut tx: Sender<ChunkedFile>,
) -> Result<()> {
    let abs_path = worktree_root.join(&entry.path);

    let text = fs.load(&abs_path).await?;
    let language = language_registry
        .language_for_file_path(&entry.path)
        .await
        .with_context(|| format!("selecting a language for {:?}", entry.path))?;
    if let Some(grammar) = language.grammar() {
        let tree = PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            parser
                .set_language(&grammar.ts_language)
                .expect("incompatible grammar");
            parser.parse(&text, None).expect("invalid language")
        });

        let chunks = chunk_parse_tree(tree, &text);

        tx.send(ChunkedFile {
            worktree_root,
            entry,
            text,
            chunks,
        })
        .await?;
    } else {
        return Err(anyhow!("plain text is not yet supported"));
    }

    Ok(())
}

const CHUNK_THRESHOLD: usize = 1500;

fn chunk_parse_tree(tree: Tree, text: &str) -> Vec<Chunk> {
    let mut chunk_ranges = Vec::new();
    let mut cursor = tree.walk();

    let mut range = 0..0;
    loop {
        let node = cursor.node();

        // If adding the node to the current chunk exceeds the threshold
        if node.end_byte() - range.start > CHUNK_THRESHOLD {
            // Try to descend into its first child, and if we can't flush the current
            // range and try again.
            if cursor.goto_first_child() {
                continue;
            } else if !range.is_empty() {
                chunk_ranges.push(range.clone());
                range.start = range.end;
                continue;
            }

            // If we get here, the node itself has no children but is larger than the threshold.
            // Break its text into arbitrary chunks.
            while range.start < node.end_byte() {
                range.end = cmp::min(range.start + CHUNK_THRESHOLD, node.end_byte());
                while !text.is_char_boundary(range.end) {
                    range.end -= 1;
                }
                chunk_ranges.push(range.clone());
                range.start = range.end;
            }
        } else {
            // The current node fits the threshold, so we include it wholesale.
            range.end = node.end_byte();
        }

        // If we get here, we consumed the node. Advance to the next child, ascending if there isn't one.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                if !range.is_empty() {
                    chunk_ranges.push(range);
                }

                return chunk_ranges
                    .into_iter()
                    .map(|range| {
                        let mut hasher = Sha256::new();
                        hasher.update(&text[range.clone()]);
                        let mut digest = [0u8; 32];
                        digest.copy_from_slice(hasher.finalize().as_slice());
                        Chunk { range, digest }
                    })
                    .collect();
            }
        }
    }
}

fn embed_chunks(
    chunked_files: Receiver<ChunkedFile>,
    cx: &mut ModelContext<ProjectIndex>,
) -> Task<()> {
    cx.spawn(|this, cx| async move {
        let mut chunked_file_batches = chunked_files.chunks_timeout(512, Duration::from_secs(2));
        while let Some(batch) = chunked_file_batches.next().await {
            dbg!(batch.len());
        }
    })
}
