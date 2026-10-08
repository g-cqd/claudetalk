//! Store actors: each owns one `Store` on a dedicated OS thread and runs jobs sent
//! over a bounded tokio mpsc queue (capacity 1024), answering on a oneshot.

use crate::DaemonError;
use agora_proto::Event;
use agora_store::{Store, StoreError};
use std::panic::{self, AssertUnwindSafe};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Capacity of each actor's job queue; a full queue makes `exec` wait for room.
const QUEUE_CAPACITY: usize = 1024;

/// A job for the writer thread. It owns its reply channel and publishes its events.
type WriteJob = Box<dyn FnOnce(&mut Store) + Send>;
/// A job for the reader thread. It owns its reply channel.
type ReadJob = Box<dyn FnOnce(&Store) + Send>;

/// Maps a job that panicked to the error its caller sees. The thread keeps serving.
fn internal_panic(actor: &'static str) -> DaemonError {
    tracing::error!(actor, "store job panicked; the actor thread keeps serving");
    DaemonError::Store(StoreError::Invalid("internal panic"))
}

/// Handle to the writer thread (the only read-write connection). Clone freely.
#[derive(Clone)]
pub struct Writer {
    jobs: mpsc::Sender<WriteJob>,
    bus: broadcast::Sender<Event>,
}

impl Writer {
    /// Start the writer thread (named "agora-writer"). The thread ends when every
    /// `Writer` clone is dropped and the queue is drained; joining the handle drops the Store.
    pub fn spawn(
        store: Store,
        bus: broadcast::Sender<Event>,
    ) -> (Writer, std::thread::JoinHandle<()>) {
        let (jobs, mut queue) = mpsc::channel::<WriteJob>(QUEUE_CAPACITY);
        let handle = std::thread::Builder::new()
            .name("agora-writer".into())
            .spawn(move || {
                let mut store = store;
                while let Some(job) = queue.blocking_recv() {
                    job(&mut store);
                }
            })
            .expect("the OS refused to start the agora-writer thread");
        (Writer { jobs, bus }, handle)
    }

    /// Run `f` on the writer thread. On `Ok((value, events))` the events are sent on
    /// the bus in order (send errors = no subscribers, ignored) BEFORE the reply is
    /// delivered, so a caller that sees the reply also sees the events published.
    /// `Err(e)` -> `DaemonError::Store(e)`; thread gone -> `DaemonError::ActorGone`.
    /// A panic inside `f` must not kill the thread: wrap the call in
    /// `catch_unwind(AssertUnwindSafe(..))`, publish nothing, and answer
    /// `DaemonError::Store(StoreError::Invalid("internal panic"))` (logged at error).
    pub async fn exec<T, F>(&self, f: F) -> Result<T, DaemonError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<(T, Vec<Event>), StoreError> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel::<Result<T, DaemonError>>();
        let bus = self.bus.clone();
        let job: WriteJob = Box::new(move |store: &mut Store| {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| f(store))) {
                Ok(Ok((value, events))) => {
                    for event in events {
                        // No subscribers is not an error: nobody is waiting for the event.
                        let _ = bus.send(event);
                    }
                    Ok(value)
                }
                Ok(Err(e)) => Err(DaemonError::Store(e)),
                Err(_) => Err(internal_panic("writer")),
            };
            // The caller may have stopped waiting; the work is committed either way.
            let _ = reply.send(result);
        });
        self.jobs
            .send(job)
            .await
            .map_err(|_| DaemonError::ActorGone)?;
        answer.await.map_err(|_| DaemonError::ActorGone)?
    }
}

/// Handle to the reader thread (a second connection, used only through `&Store`).
#[derive(Clone)]
pub struct Reader {
    jobs: mpsc::Sender<ReadJob>,
}

impl Reader {
    /// Start the reader thread (named "agora-reader"); same lifecycle as `Writer::spawn`.
    pub fn spawn(store: Store) -> (Reader, std::thread::JoinHandle<()>) {
        let (jobs, mut queue) = mpsc::channel::<ReadJob>(QUEUE_CAPACITY);
        let handle = std::thread::Builder::new()
            .name("agora-reader".into())
            .spawn(move || {
                while let Some(job) = queue.blocking_recv() {
                    job(&store);
                }
            })
            .expect("the OS refused to start the agora-reader thread");
        (Reader { jobs }, handle)
    }

    /// Run `f` on the reader thread; error mapping and panic handling as `Writer::exec`.
    pub async fn exec<T, F>(&self, f: F) -> Result<T, DaemonError>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel::<Result<T, DaemonError>>();
        let job: ReadJob = Box::new(move |store: &Store| {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| f(store))) {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(e)) => Err(DaemonError::Store(e)),
                Err(_) => Err(internal_panic("reader")),
            };
            let _ = reply.send(result);
        });
        self.jobs
            .send(job)
            .await
            .map_err(|_| DaemonError::ActorGone)?;
        answer.await.map_err(|_| DaemonError::ActorGone)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Upper bound on every wait, so a bug fails the test instead of hanging it.
    const WAIT: Duration = Duration::from_secs(5);

    fn open() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    /// Asserts the actor thread exits (without panicking) once its handles are gone.
    async fn assert_joins(handle: std::thread::JoinHandle<()>) {
        let joined = timeout(WAIT, tokio::task::spawn_blocking(move || handle.join()))
            .await
            .expect("actor thread did not exit after its last handle was dropped")
            .expect("join task");
        assert!(joined.is_ok(), "actor thread panicked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_exec_returns_the_value() {
        let (bus, _) = broadcast::channel::<Event>(16);
        let (writer, handle) = Writer::spawn(open(), bus);
        let value = timeout(WAIT, writer.exec(|_| Ok((42_i64, Vec::new()))))
            .await
            .expect("exec did not hang");
        assert_eq!(value.expect("job succeeds"), 42);
        drop(writer);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_events_are_queued_on_the_bus_before_exec_returns() {
        let (bus, _) = broadcast::channel::<Event>(16);
        let mut rx = bus.subscribe();
        let (writer, handle) = Writer::spawn(open(), bus);
        let event = Event::MessageAppended {
            channel: "general".into(),
            seq: 7,
        };
        writer
            .exec(move |_| Ok((7_i64, vec![event])))
            .await
            .expect("job succeeds");
        // No await between exec returning and this receive: the event is already queued.
        let got = rx.try_recv().expect("event was published before the reply");
        assert!(matches!(got, Event::MessageAppended { seq: 7, .. }));
        drop(writer);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_store_error_maps_to_daemon_store_error() {
        let (bus, _) = broadcast::channel::<Event>(16);
        let (writer, handle) = Writer::spawn(open(), bus);
        let err = writer
            .exec(|_| -> Result<(i64, Vec<Event>), StoreError> {
                Err(StoreError::NotMember("general".into()))
            })
            .await
            .expect_err("store error is an error");
        assert!(matches!(err, DaemonError::Store(StoreError::NotMember(_))));
        drop(writer);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_panicking_job_is_answered_and_next_job_runs() {
        let (bus, _) = broadcast::channel::<Event>(16);
        let (writer, handle) = Writer::spawn(open(), bus);
        let err = writer
            .exec(|_| -> Result<(i64, Vec<Event>), StoreError> { panic!("boom") })
            .await
            .expect_err("a panicking job is an error");
        assert!(matches!(err, DaemonError::Store(StoreError::Invalid(_))));
        let value = writer
            .exec(|_| Ok((5_i64, Vec::new())))
            .await
            .expect("the thread survived the panic");
        assert_eq!(value, 5);
        drop(writer);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_thread_joins_only_after_the_last_clone_is_dropped() {
        let (bus, _) = broadcast::channel::<Event>(16);
        let (writer, handle) = Writer::spawn(open(), bus);
        let clone = writer.clone();
        drop(writer);
        // The surviving clone keeps the queue open, so the thread cannot have exited.
        assert!(!handle.is_finished());
        clone
            .exec(|_| Ok((1_i64, Vec::new())))
            .await
            .expect("clone still works");
        drop(clone);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reader_exec_returns_value_and_maps_store_error() {
        let (reader, handle) = Reader::spawn(open());
        let value = reader.exec(|_| Ok(9_i64)).await.expect("job succeeds");
        assert_eq!(value, 9);
        let err = reader
            .exec(|_| -> Result<i64, StoreError> { Err(StoreError::NotMember("general".into())) })
            .await
            .expect_err("store error is an error");
        assert!(matches!(err, DaemonError::Store(StoreError::NotMember(_))));
        drop(reader);
        assert_joins(handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reader_panicking_job_is_answered_and_next_job_runs() {
        let (reader, handle) = Reader::spawn(open());
        let err = reader
            .exec(|_| -> Result<i64, StoreError> { panic!("boom") })
            .await
            .expect_err("a panicking job is an error");
        assert!(matches!(err, DaemonError::Store(StoreError::Invalid(_))));
        let value = reader
            .exec(|_| Ok(3_i64))
            .await
            .expect("the thread survived the panic");
        assert_eq!(value, 3);
        drop(reader);
        assert_joins(handle).await;
    }
}
