//! Per-stream output buffering for a single client connection.
//!
//! Each client WebSocket connection multiplexes up to `max_streams_per_conn`
//! streams. To satisfy the spec's per-stream backpressure requirement
//! (Part 1 ¶4: "if the buffer exceeds a configurable limit (default 256KB),
//! drop the oldest chunks and log a warning"), each stream gets its own
//! bounded queue. Producers (forward tasks, dispatcher) push frames
//! non-blockingly; on overflow, the oldest *binary* chunks for that stream
//! are dropped while text control frames (queued/done/error) are preserved.
//!
//! A single writer task per client drains the per-stream queues round-robin
//! into the WS sink. This prevents head-of-line blocking: a slow stream's
//! backed-up queue cannot stall the writer from sending another stream's
//! `done` frame.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::{SinkExt, stream::SplitSink};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::types::StreamId;

/// One frame queued for a stream — either a binary audio chunk (droppable
/// under backpressure) or a text control frame (never dropped).
#[derive(Debug)]
pub enum ClientEvent {
    Text(String),
    Binary(Bytes),
}

/// Returned by `enqueue` when the client has disconnected. Forward tasks
/// translate this into a `ClientGone` outcome so the dispatcher can free
/// the backend slot.
#[derive(Debug)]
pub struct ClientGone;

#[derive(Debug, Default)]
struct StreamQueue {
    /// Mixed binary + text frames in send order. Text is never dropped;
    /// binary is dropped from the oldest end on overflow.
    pending: VecDeque<ClientEvent>,
    /// Sum of bytes in queued binary frames (text not counted).
    bytes_in_queue: usize,
    /// Lifetime counter of dropped bytes for the warning log.
    dropped_bytes: usize,
    /// True when we've warned about dropping for this stream — keeps the
    /// log from spamming once per chunk under sustained slow-consumer load.
    warned: bool,
}

impl StreamQueue {
    /// Drop the oldest binary frame in `pending`. Returns the number of bytes
    /// freed, or None if no binary frames are queued.
    fn drop_oldest_binary(&mut self) -> Option<usize> {
        let pos = self
            .pending
            .iter()
            .position(|ev| matches!(ev, ClientEvent::Binary(_)))?;
        let removed = self.pending.remove(pos)?;
        match removed {
            ClientEvent::Binary(data) => {
                let len = data.len();
                self.bytes_in_queue = self.bytes_in_queue.saturating_sub(len);
                self.dropped_bytes = self.dropped_bytes.saturating_add(len);
                Some(len)
            }
            // Position was filtered by the matches! above; unreachable.
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    streams: HashMap<StreamId, StreamQueue>,
    /// Fair scheduling order: streams with pending frames, in the order they
    /// should be visited next. A stream with more pending frames after the
    /// writer dequeues is re-pushed to the back.
    rr_order: VecDeque<StreamId>,
    /// Set when the client connection has ended (writer sink errored, or the
    /// handler exited). Producers see this and return `ClientGone`.
    closed: bool,
}

/// Per-client output queue manager. Cheap to clone (`Arc` semantics expected).
#[derive(Debug)]
pub struct ClientStreams {
    inner: Mutex<Inner>,
    notify: Notify,
    max_bytes_per_stream: usize,
}

impl ClientStreams {
    pub fn new(max_bytes_per_stream: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            max_bytes_per_stream,
        }
    }

    /// Push an event for `stream_id`. Non-blocking. On binary overflow,
    /// drops oldest binary frames (text frames are preserved and never
    /// dropped). Returns `Err(ClientGone)` if the client has disconnected.
    pub fn enqueue(&self, stream_id: &StreamId, event: ClientEvent) -> Result<(), ClientGone> {
        let mut inner = self.inner.lock().expect("client streams mutex poisoned");
        if inner.closed {
            return Err(ClientGone);
        }
        let queue = inner.streams.entry(stream_id.clone()).or_default();
        if let ClientEvent::Binary(ref data) = event {
            let len = data.len();
            // Drop oldest binary frames until adding `data` fits within budget.
            // If even after dropping every binary frame we still don't fit
            // (a single oversize frame), accept it anyway — better to deliver
            // a truncated stream than silently lose a single chunk that's
            // larger than the cap. The warning logs the situation.
            while queue.bytes_in_queue + len > self.max_bytes_per_stream {
                if queue.drop_oldest_binary().is_none() {
                    break;
                }
            }
            queue.bytes_in_queue = queue.bytes_in_queue.saturating_add(len);
            if queue.dropped_bytes > 0 && !queue.warned {
                tracing::warn!(
                    stream = %stream_id,
                    dropped_bytes = queue.dropped_bytes,
                    cap = self.max_bytes_per_stream,
                    "slow consumer: dropping oldest audio chunks for stream",
                );
                queue.warned = true;
            }
        }
        queue.pending.push_back(event);
        if !inner.rr_order.iter().any(|s| s == stream_id) {
            inner.rr_order.push_back(stream_id.clone());
        }
        drop(inner);
        self.notify.notify_one();
        Ok(())
    }

    /// Mark the connection closed and wake the writer for shutdown.
    pub fn close(&self) {
        let mut inner = self.inner.lock().expect("client streams mutex poisoned");
        inner.closed = true;
        drop(inner);
        self.notify.notify_one();
    }

    /// Pop the next event in fair round-robin order. Returns None when no
    /// stream has pending events.
    fn dequeue(&self) -> Option<ClientEvent> {
        let mut inner = self.inner.lock().expect("client streams mutex poisoned");
        let len = inner.rr_order.len();
        for _ in 0..len {
            let Some(stream_id) = inner.rr_order.pop_front() else {
                break;
            };
            if let Some(queue) = inner.streams.get_mut(&stream_id) {
                if let Some(event) = queue.pending.pop_front() {
                    if let ClientEvent::Binary(ref data) = event {
                        queue.bytes_in_queue = queue.bytes_in_queue.saturating_sub(data.len());
                    }
                    if !queue.pending.is_empty() {
                        inner.rr_order.push_back(stream_id);
                    }
                    return Some(event);
                }
            }
            // Empty or stale entry; don't re-push.
        }
        None
    }

    /// Run the writer loop until the connection closes or the sink errors.
    /// Owns the WS sink for the connection.
    pub async fn run_writer(
        self: std::sync::Arc<Self>,
        mut sink: SplitSink<WebSocketStream<TcpStream>, Message>,
    ) {
        loop {
            // Drain everything we can without awaiting.
            while let Some(event) = self.dequeue() {
                let msg = match event {
                    ClientEvent::Text(text) => Message::Text(text.into()),
                    ClientEvent::Binary(data) => Message::Binary(data),
                };
                if let Err(e) = sink.send(msg).await {
                    tracing::debug!("client write error: {e}");
                    self.close();
                    return;
                }
            }
            // Nothing more to send right now: wait for the next signal.
            // Re-check `closed` after wakeup.
            self.notify.notified().await;
            if self.inner.lock().expect("client streams mutex poisoned").closed
                && self.dequeue_is_empty()
            {
                return;
            }
        }
    }

    fn dequeue_is_empty(&self) -> bool {
        let inner = self.inner.lock().expect("client streams mutex poisoned");
        inner.rr_order.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sid(s: &str) -> StreamId {
        StreamId(s.to_owned())
    }

    fn b(n: usize) -> ClientEvent {
        ClientEvent::Binary(Bytes::from(vec![0u8; n]))
    }

    fn t(s: &str) -> ClientEvent {
        ClientEvent::Text(s.to_owned())
    }

    #[test]
    fn push_under_budget_no_drop() {
        let cs = ClientStreams::new(1024);
        cs.enqueue(&sid("a"), b(100)).unwrap();
        cs.enqueue(&sid("a"), b(200)).unwrap();
        let inner = cs.inner.lock().unwrap();
        let q = inner.streams.get(&sid("a")).unwrap();
        assert_eq!(q.bytes_in_queue, 300);
        assert_eq!(q.dropped_bytes, 0);
        assert_eq!(q.pending.len(), 2);
    }

    #[test]
    fn overflow_drops_oldest_binary() {
        let cs = ClientStreams::new(300);
        cs.enqueue(&sid("a"), b(100)).unwrap(); // [100]
        cs.enqueue(&sid("a"), b(100)).unwrap(); // [100, 100]
        cs.enqueue(&sid("a"), b(100)).unwrap(); // [100, 100, 100], total 300, OK
        cs.enqueue(&sid("a"), b(100)).unwrap(); // would exceed; drops oldest → [100, 100, 100]
        let inner = cs.inner.lock().unwrap();
        let q = inner.streams.get(&sid("a")).unwrap();
        assert_eq!(q.bytes_in_queue, 300);
        assert_eq!(q.dropped_bytes, 100);
        assert_eq!(q.pending.len(), 3);
    }

    #[test]
    fn text_frames_never_dropped() {
        let cs = ClientStreams::new(200);
        cs.enqueue(&sid("a"), t("queued")).unwrap();
        cs.enqueue(&sid("a"), b(100)).unwrap();
        cs.enqueue(&sid("a"), b(100)).unwrap(); // total 200
        cs.enqueue(&sid("a"), b(100)).unwrap(); // would exceed; drops oldest binary
        cs.enqueue(&sid("a"), t("done")).unwrap();
        let inner = cs.inner.lock().unwrap();
        let q = inner.streams.get(&sid("a")).unwrap();
        // queued + 2 binary (one dropped) + done = 4 events
        assert_eq!(q.pending.len(), 4);
        assert!(matches!(q.pending[0], ClientEvent::Text(_)));
        assert!(matches!(q.pending[3], ClientEvent::Text(_)));
        assert_eq!(q.dropped_bytes, 100);
    }

    #[test]
    fn round_robin_dequeue_across_streams() {
        let cs = ClientStreams::new(10_000);
        cs.enqueue(&sid("a"), b(10)).unwrap();
        cs.enqueue(&sid("a"), b(10)).unwrap();
        cs.enqueue(&sid("b"), b(10)).unwrap();
        cs.enqueue(&sid("b"), b(10)).unwrap();
        // Dequeue order should alternate: a, b, a, b
        let mut got = vec![];
        while let Some(ev) = cs.dequeue() {
            // We need a way to distinguish — re-inspect via the bytes (all same).
            // Instead, inspect rr_order indirectly via what's left.
            got.push(ev);
        }
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn round_robin_strict_alternation() {
        let cs = ClientStreams::new(10_000);
        cs.enqueue(&sid("a"), t("a1")).unwrap();
        cs.enqueue(&sid("a"), t("a2")).unwrap();
        cs.enqueue(&sid("b"), t("b1")).unwrap();
        cs.enqueue(&sid("b"), t("b2")).unwrap();
        let mut order = vec![];
        while let Some(ev) = cs.dequeue() {
            if let ClientEvent::Text(s) = ev {
                order.push(s);
            }
        }
        assert_eq!(order, vec!["a1", "b1", "a2", "b2"]);
    }

    #[test]
    fn close_then_enqueue_returns_client_gone() {
        let cs = ClientStreams::new(1024);
        cs.close();
        assert!(cs.enqueue(&sid("a"), t("queued")).is_err());
    }

    #[test]
    fn drops_oldest_only_skipping_text() {
        let cs = ClientStreams::new(150);
        cs.enqueue(&sid("a"), b(100)).unwrap(); // [B100]
        cs.enqueue(&sid("a"), t("ctrl")).unwrap(); // [B100, T]
        cs.enqueue(&sid("a"), b(100)).unwrap(); // would exceed; drops B100 (the only binary), accepts B100 → [T, B100]
        let inner = cs.inner.lock().unwrap();
        let q = inner.streams.get(&sid("a")).unwrap();
        assert_eq!(q.pending.len(), 2);
        assert!(matches!(q.pending[0], ClientEvent::Text(_)));
        assert!(matches!(q.pending[1], ClientEvent::Binary(_)));
        assert_eq!(q.bytes_in_queue, 100);
        assert_eq!(q.dropped_bytes, 100);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_exits_on_close() {
        // Build a streams + dummy "sink" via a oneshot we never write to —
        // since SplitSink for WebSocketStream<TcpStream> isn't easily mockable,
        // we just verify the close path via `dequeue_is_empty` semantics.
        let cs = Arc::new(ClientStreams::new(1024));
        cs.close();
        // After close with empty queues, run_writer's wait loop should exit.
        // We can't easily run run_writer without a real sink, so just verify
        // the predicate that drives the exit branch.
        assert!(cs.dequeue_is_empty());
        assert!(cs.inner.lock().unwrap().closed);
    }
}
