use std::sync::Arc;
use std::path::PathBuf;
use tokio::{io, fs};
use crate::root::{GetRootEntryError, ConnectedRoot};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{channel, unbounded_channel, Sender, UnboundedSender, Receiver, UnboundedReceiver};
use tokio::sync::oneshot::{channel as oneshot_channel, Sender as OneshotSender};
use std::sync::atomic::{AtomicUsize, Ordering, AtomicBool};
use tokio::select;
use tokio::task::spawn;
use thiserror::Error;
use tokio::fs::DirEntry;
use rusqlite::params;

#[derive(Debug, Error)]
#[error("couldn't index at {path}: {error}")]
pub struct NonFatalIndexError {
    path: PathBuf,
    error: io::Error,
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("sqlite connection error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("failed to get root dir entry: {0}")]
    GetRootDir(#[from] GetRootEntryError),

    #[error("path wasn't properly encoded utf8")]
    Utf8,

    #[error("couldn't return valid id as channel went offline")]
    GetId,
}


#[derive(Debug, Clone)]
pub struct Task {
    path: PathBuf,
    parent_id: i64,
}

pub struct Inner {
    errors: Mutex<Vec<NonFatalIndexError>>,
    fatal_errors_tx: Sender<IndexError>,
    db_tx: Sender<DbMessage>,
    todo_queue_tx: UnboundedSender<Task>,

    done_first: AtomicBool,
    done: AtomicUsize,
    queued: AtomicUsize,
    spawned: AtomicUsize,
    root_id: i64,
    pub task_done_tx: Sender<()>,
}

impl Inner {
    async fn index_direntry(&self, entry: DirEntry, parent_id: i64) -> i64 {
        let (resp_tx, resp_rx) = oneshot_channel();

        let path = entry.path();

        if let Err(err) = self.db_tx.send(DbMessage {
            resp: resp_tx,
            entry,
            parent_id,
        }).await {
            log::error!("couldn't send db msg {:?}", err)
        };

        match resp_rx.await {
            Ok(i) => {
                log::debug!("received response (id={}) from path {:?}", i, path);
                i
            },
            Err(_) => {
                // resp_tx is dropped, just return 0 and raise a fatal error
                if let Err(err) = self.fatal_errors_tx.send(IndexError::GetId).await {
                    log::error!("couldn't send fatal error msg {}", err);
                }
                0
            }
        }
    }

    pub(self) async fn process_task(&self, task: Task) -> Result<(), NonFatalIndexError> {
        macro_rules! non_fatal {
            ($($tt: tt)*) => {
                match {$($tt)*} {
                    Ok(i) => i,
                    Err(e) => {
                        return Err(NonFatalIndexError {
                            path: task.path,
                            error: e,
                        });
                    }
                }
            };
        }

        let mut dir = non_fatal!(fs::read_dir(&task.path).await);
        while let Some(entry) = non_fatal!(dir.next_entry().await) {
            let path = entry.path();
            let identifier = self.index_direntry(entry, task.parent_id).await;

            log::debug!("indexed direntry at {:?}", path);

            if path.is_dir() {
                if let Err(err) = self.todo_queue_tx.send(Task {
                    path,
                    parent_id: identifier
                }) {
                    log::error!("couldn't send new task msg {}", err);
                }
                self.queued.fetch_add(1, Ordering::SeqCst);
            }
        }

        if task.parent_id == self.root_id {
            self.done_first.store(true, Ordering::SeqCst);
        }

        log::debug!("processed task with path {:?}", task.path);

        Ok(())
    }
}

#[derive(Debug)]
struct DbMessage {
    resp: OneshotSender<i64>,
    entry: DirEntry,
    parent_id: i64,
}

pub struct Indexer<'dfs, 'root> {
    inner: Arc<Inner>,

    // Option cause we will move it out of the struct and need to replace it with something.
    fatal_errors_rx: Option<Receiver<IndexError>>,
    // Option cause we will move it out of the struct and need to replace it with something.
    task_done_rx: Option<Receiver<()>>,
    // Option cause we will move it out of the struct and need to replace it with something.
    db_rx: Option<Receiver<DbMessage>>,

    // There will never actually be contention over this mutex
    // because it will never be accessed concurrently.
    todo_queue_rx: Mutex<UnboundedReceiver<Task>>,

    // There will never actually be contention over this mutex
    // because it will never be accessed concurrently.
    root: Mutex<&'root mut ConnectedRoot<'dfs>>
}

impl<'dfs, 'root> Indexer<'dfs, 'root> {
    pub(crate) fn new(root: &'root mut ConnectedRoot<'dfs>) -> Result<Self, IndexError> {
        let errors = Vec::new();
        let (fatal_errors_tx, fatal_errors_rx) = channel(1);
        let (todo_queue_tx, todo_queue_rx) = unbounded_channel();
        // TODO: configure the 20
        let (db_tx, db_rx) = channel(1024);
        let (task_done_tx, task_done_rx) = channel(1024);

        let root_id = root.root_dir()?.id();
        if let Err(err) = todo_queue_tx.send(Task {
            path: root.path().clone(),
            parent_id: root_id
        }) {
            log::error!("couldn't send initial task in todo queue: {}", err);
        }

        Ok(Self {
            inner: Arc::new(Inner {
                errors: Mutex::new(errors),
                fatal_errors_tx,
                db_tx,
                task_done_tx,
                todo_queue_tx,
                done_first: AtomicBool::new(false),
                done: AtomicUsize::new(0),
                // one is queued already at the start (the root)
                queued: AtomicUsize::new(1),
                spawned: AtomicUsize::new(0),
                root_id
            }),
            fatal_errors_rx: Some(fatal_errors_rx),
            task_done_rx: Some(task_done_rx),
            db_rx: Some(db_rx),
            todo_queue_rx: Mutex::new(todo_queue_rx),
            root: Mutex::new(root),
        })
    }

    async fn do_index(&self, no_next_task: OneshotSender<()>) {
        let next_task = self.todo_queue_rx.lock().await.recv().await;
        if let Some(i) = next_task {
            let inner = Arc::clone(&self.inner);

            inner.spawned.fetch_add(1, Ordering::SeqCst);
            spawn(async move {
                if let Err(e) = inner.process_task(i.clone()).await {
                    inner.errors.lock().await.push(e);
                }

                inner.done.fetch_add(1, Ordering::SeqCst);
                if let Err(err) = inner.task_done_tx.send(()).await {
                    log::error!("failed to send task done message: {}", err);
                };
            });
        } else if no_next_task.send(()).is_err() {
            log::error!("couldn't send no next task msg")
        }
    }

    async fn handle_db_message(&self, msg: DbMessage) -> Result<(), IndexError> {
        let mut root = self.root.lock().await;
        let tx = root.connection.transaction()?;
        tx.execute(
            "INSERT into files (name, parent, dir) VALUES (?1, ?2, ?3)",
            params![
                msg.entry.file_name().to_str().ok_or(IndexError::Utf8)?,
                msg.parent_id,
                msg.entry.path().to_str().ok_or(IndexError::Utf8)?,
            ]
        )?;
        let id = tx.last_insert_rowid();

        tx.commit()?;

        if let Err(err) = msg.resp.send(id) {
            log::error!("couldn't send response (id={})", err)
        };

        Ok(())
    }

    pub async fn index(mut self) -> Result<(), IndexError> {
        // unwrap safe because we can only call index once
        let mut fatal_error = self.fatal_errors_rx.take().unwrap();
        // unwrap safe because we can only call index once
        let mut task_done_rx = self.task_done_rx.take().unwrap();
        // unwrap safe because we can only call index once
        let mut db_rx = self.db_rx.take().unwrap();

        let (no_next_task_tx,mut no_next_task_rx) = oneshot_channel();
        let mut index_fut_task = Box::pin(self.do_index(no_next_task_tx));

        loop {
            select!{
                biased;
                _ = &mut no_next_task_rx => {
                    break
                },
                _ = task_done_rx.recv() => {
                    let done = self.inner.done.load(Ordering::SeqCst);
                    let queued = self.inner.queued.load(Ordering::SeqCst);
                    let spawned = self.inner.spawned.load(Ordering::SeqCst);
                    let done_first = self.inner.done_first.load(Ordering::SeqCst);
                    let todo = queued - done;
                    let doing = spawned - done;

                    log::info!("total_queued: {} todo: {}, done: {}, doing: {}, done_first: {}", queued, todo, done, doing, done_first);

                    if todo == 0 && doing == 0 && done_first {
                        break;
                    }
                }
                err = fatal_error.recv() => if let Some(e) = err {
                    return Err(e)
                },
                msg = db_rx.recv() => if let Some(msg) = msg {
                    self.handle_db_message(msg).await?;
                },
                _ = &mut index_fut_task => {
                    let (no_next_task_tx, new_no_next_task_rx) = oneshot_channel();
                    no_next_task_rx = new_no_next_task_rx;

                    index_fut_task = Box::pin(self.do_index(no_next_task_tx));
                },
            }
        };

        log::info!("done");
        Ok(())
    }
}