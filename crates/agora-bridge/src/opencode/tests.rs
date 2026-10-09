//! The busy gate of `deliver_loop`, with paused time and no socket: the prompt sink records the
//! prompts instead of posting them, and the clock moves only where a test advances it.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const SESSION: &str = "ses_unit";

/// Records each prompt instead of posting it. With `refuse` set, `post_prompt` fails and records
/// nothing, as a refused POST would.
#[derive(Default)]
struct Recorder {
    posted: Mutex<Vec<String>>,
    refuse: AtomicBool,
}

impl Recorder {
    fn posted(&self) -> Vec<String> {
        self.posted.lock().expect("log").clone()
    }
}

impl PromptSink for Recorder {
    async fn post_prompt(&self, _session_id: &str, text: &str) -> Result<(), BridgeError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(BridgeError::Http("refused".into()));
        }
        self.posted.lock().expect("log").push(text.to_owned());
        Ok(())
    }
}

/// A running `deliver_loop` whose busy state the test sets.
struct Gate {
    sink: Arc<Recorder>,
    busy: Arc<watch::Sender<bool>>,
    deliveries: mpsc::Sender<Delivery>,
    stop: CancellationToken,
    task: JoinHandle<Result<(), BridgeError>>,
}

impl Gate {
    /// Stops the loop and checks that it ends cleanly.
    async fn finish(self) {
        self.stop.cancel();
        self.task
            .await
            .expect("loop task did not panic")
            .expect("loop returns Ok");
    }
}

fn spawn_gate(busy_max_wait: Duration) -> Gate {
    let sink = Arc::new(Recorder::default());
    let busy = Arc::new(watch::channel(false).0);
    let mut busy_rx = busy.subscribe();
    let (deliveries, mut rx) = mpsc::channel(4);
    let stop = CancellationToken::new();
    let task = tokio::spawn({
        let (sink, busy, stop) = (Arc::clone(&sink), Arc::clone(&busy), stop.clone());
        async move {
            deliver_loop(
                &*sink,
                SESSION,
                &mut rx,
                &mut busy_rx,
                busy_max_wait,
                &busy,
                &stop,
            )
            .await
        }
    });
    Gate {
        sink,
        busy,
        deliveries,
        stop,
        task,
    }
}

/// Lets every task that can run without time passing reach its next wait. Yielding never parks
/// the runtime, so the paused clock does not move here.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// A delivery with the text `text`, and the receiver of its answer.
fn delivery(text: &str) -> (Delivery, oneshot::Receiver<bool>) {
    let (reply, answer) = oneshot::channel();
    let delivery = Delivery {
        text: text.to_owned(),
        max_seq: 1,
        count: 1,
        reply,
    };
    (delivery, answer)
}

#[tokio::test(start_paused = true)]
async fn a_delivery_is_held_while_busy_and_goes_out_once_idle() {
    let gate = spawn_gate(BUSY_MAX_WAIT);
    gate.busy.send_replace(true);
    let (delivery, mut answer) = delivery("first");
    gate.deliveries
        .send(delivery)
        .await
        .expect("loop is running");
    settle().await;

    // Nine of the ten minutes of the bound pass while the session stays busy.
    tokio::time::advance(BUSY_MAX_WAIT - Duration::from_secs(60)).await;
    settle().await;
    assert!(gate.sink.posted().is_empty(), "posted while busy");
    assert!(
        matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "answered while busy"
    );

    gate.busy.send_replace(false);
    let accepted = tokio::time::timeout(Duration::from_secs(1), answer)
        .await
        .expect("answered once idle")
        .expect("reply sent");
    assert!(accepted, "accepted once idle");
    assert_eq!(gate.sink.posted(), ["first"]);
    gate.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_refused_prompt_is_answered_false_and_clears_busy() {
    let gate = spawn_gate(BUSY_MAX_WAIT);
    gate.sink.refuse.store(true, Ordering::SeqCst);
    let (delivery, answer) = delivery("refused");
    gate.deliveries
        .send(delivery)
        .await
        .expect("loop is running");

    // The session is idle, so the loop marks it busy, posts, and the refused POST clears the mark.
    let accepted = tokio::time::timeout(Duration::from_secs(1), answer)
        .await
        .expect("answered after the refusal")
        .expect("reply sent");
    assert!(!accepted, "refused prompt is not accepted");
    assert!(!*gate.busy.borrow(), "busy cleared after the refusal");
    assert!(gate.sink.posted().is_empty(), "nothing recorded as posted");
    gate.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_session_that_stays_busy_gets_the_prompt_once_the_bound_passes() {
    let gate = spawn_gate(BUSY_MAX_WAIT);
    gate.busy.send_replace(true);
    let (delivery, answer) = delivery("stuck");
    gate.deliveries
        .send(delivery)
        .await
        .expect("loop is running");
    settle().await;

    tokio::time::advance(BUSY_MAX_WAIT - Duration::from_secs(1)).await;
    settle().await;
    assert!(gate.sink.posted().is_empty(), "posted before the bound");

    // Past the bound the loop posts anyway, and the sink's 2xx is the answer.
    tokio::time::advance(Duration::from_secs(2)).await;
    let accepted = tokio::time::timeout(Duration::from_secs(1), answer)
        .await
        .expect("answered after the bound")
        .expect("reply sent");
    assert!(accepted, "accepted after the bound");
    assert_eq!(gate.sink.posted(), ["stuck"]);
    gate.finish().await;
}
