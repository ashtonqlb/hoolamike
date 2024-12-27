use {
    super::nested_archive_manager::WithPermit,
    crate::compression::{ArchiveHandle, ProcessArchive, SeekWithTempFileExt},
    futures::{FutureExt, TryFutureExt},
    nonempty::NonEmpty,
    std::{
        convert::identity,
        future::{ready, Future},
        path::PathBuf,
        pin::Pin,
        sync::Arc,
    },
    tap::prelude::*,
    tokio::{
        sync::{watch::error::RecvError, AcquireError, Semaphore},
        task::JoinHandle,
    },
    tracing::{debug, debug_span, instrument, Instrument},
};

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    #[error("channel has been closed")]
    ChannelClosed(#[source] RecvError),
    #[error("extraction from archive failed")]
    ExtractingFromArchive(#[source] Arc<anyhow::Error>),
    #[error("thread crashed")]
    ThreadCrashed(#[source] Arc<tokio::task::JoinError>),
    #[error("could not acquire permit")]
    AcquiringPermit(#[source] Arc<AcquireError>),
}

impl Error {
    pub fn extracting_from_archive(error: anyhow::Error) -> Self {
        error.pipe(Arc::new).pipe(Self::ExtractingFromArchive)
    }
    pub fn thread_crashed(error: tokio::task::JoinError) -> Self {
        error.pipe(Arc::new).pipe(Self::ThreadCrashed)
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

pub type Extracted = WithPermit<tempfile::TempPath>;

#[derive(Debug)]
pub enum SourceKind {
    JustPath(PathBuf),
    CachedPath(Extracted),
}

pub mod cached_future;

#[derive(Clone)]
pub struct QueuedArchiveService {
    pub tasks: Arc<cached_future::CachedFutureQueue<NonEmpty<PathBuf>, Result<Arc<SourceKind>>>>,
    pub permits: Arc<Semaphore>,
}

fn assert_send<T: Send>(t: T) -> T {
    t
}

impl QueuedArchiveService {
    pub fn new(concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            tasks: cached_future::CachedFutureQueue::new(),
            permits: Arc::new(Semaphore::new(concurrency)),
        })
    }

    pub fn get_archive_spawn(self: Arc<Self>, archive: NonEmpty<PathBuf>) -> JoinHandle<Result<Arc<SourceKind>>> {
        tokio::task::spawn(self.get_archive(archive))
    }
    async fn init_archive(self: Arc<Self>, archive_path: NonEmpty<PathBuf>) -> Result<SourceKind> {
        fn popped<T>(mut l: NonEmpty<T>) -> Option<(NonEmpty<T>, T)> {
            l.pop().map(|i| (l, i))
        }
        match popped(archive_path.clone()) {
            Some((parent, archive_path)) => {
                self.clone()
                    .pipe(assert_send)
                    .get_archive(parent)
                    .instrument(debug_span!("entry was not found, so scheduling creation of parent"))
                    .and_then(|parent| prepare_archive(self.permits.clone(), parent, archive_path))
                    .map_ok(SourceKind::CachedPath)
                    .await
            }
            None => Ok(SourceKind::JustPath(archive_path.head)),
        }
    }
    #[instrument(skip(self))]
    pub async fn get_archive(self: Arc<Self>, archive_path: NonEmpty<PathBuf>) -> Result<Arc<SourceKind>> {
        let queue = self.clone();
        tokio::task::spawn(async move {
            cloned![queue];
            self.tasks
                .clone()
                .get(archive_path, {
                    cloned![queue];
                    move |archive_path| {
                        cloned![queue];
                        queue.init_archive(archive_path).map_ok(Arc::new)
                    }
                })
                .map(|r| r.pipe_as_ref(|r| r.clone()))
                .await
        })
        .pipe(|fut| assert_send(fut))
        .await
        .map_err(self::Error::thread_crashed)
        .and_then(identity)
    }
}

#[instrument]
async fn prepare_archive(permits: Arc<Semaphore>, source: Arc<SourceKind>, archive_path: PathBuf) -> Result<Extracted> {
    let run = tracing::Span::current();
    tokio::task::spawn({
        cloned![run];
        async move {
            permits
                .acquire_owned()
                .instrument(debug_span!("acquiring file permit"))
                .map_err(Arc::new)
                .map_err(self::Error::AcquiringPermit)
                .map_ok(|permit| (source, permit))
                .and_then({
                    cloned![run];
                    move |(source, permit)| {
                        tokio::task::spawn_blocking(move || {
                            run.in_scope(|| {
                                ArchiveHandle::guess(source.as_ref().as_ref())
                                    .map_err(self::Error::extracting_from_archive)
                                    .and_then(|mut archive| {
                                        archive
                                            .get_handle(&archive_path)
                                            .map_err(self::Error::extracting_from_archive)
                                            .and_then(|mut handle| {
                                                handle
                                                    .size()
                                                    .and_then(|size| handle.seek_with_temp_file_blocking(size, permit))
                                                    .map_err(self::Error::extracting_from_archive)
                                            })
                                    })
                            })
                        })
                        .map_err(self::Error::thread_crashed)
                        .and_then(ready)
                    }
                })
                .instrument(run)
                .instrument(debug_span!("waiting for thread to finish"))
                .await
        }
    })
    .map_err(self::Error::thread_crashed)
    .and_then(ready)
    .instrument(run)
    .await
}

impl AsRef<std::path::Path> for SourceKind {
    fn as_ref(&self) -> &std::path::Path {
        match self {
            SourceKind::JustPath(path_buf) => path_buf,
            SourceKind::CachedPath(cached) => &cached.inner,
        }
    }
}
