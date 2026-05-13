use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::sync::Notify;

use crate::metrics::Metrics;
use crate::types::StreamId;

use super::buffer::{StreamBuffer, StreamEvent};

#[derive(Debug)]
pub(crate) struct RouterState {
    pub text: VecDeque<String>,
    pub streams: HashMap<StreamId, StreamBuffer>,
    pub drain_order: VecDeque<StreamId>,
    pub closed: bool,
}

#[derive(Debug)]
pub struct BinaryRouter {
    state: Mutex<RouterState>,
    notify: Notify,
    cap_bytes: usize,
}

#[derive(Clone)]
pub struct ClientChannel {
    router: Arc<BinaryRouter>,
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
pub enum WriteWork {
    Text(String),
    Chunk(Bytes),
    Terminal(String),
}

impl BinaryRouter {
    pub fn new(cap_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RouterState {
                text: VecDeque::new(),
                streams: HashMap::new(),
                drain_order: VecDeque::new(),
                closed: false,
            }),
            notify: Notify::new(),
            cap_bytes,
        })
    }

    /// Block until at least one unit of work is available; return it.
    /// Returns `None` once the channel is closed AND every queue is drained.
    pub async fn next_work(self: &Arc<Self>) -> Option<WriteWork> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);

            {
                let mut s = self.state.lock().expect("router mutex poisoned");
                if let Some(t) = s.text.pop_front() {
                    return Some(WriteWork::Text(t));
                }
                if let Some(sid) = s.drain_order.pop_front() {
                    let buf = s
                        .streams
                        .get_mut(&sid)
                        .expect("drain_order entry must have a buffer");
                    let event = buf
                        .pop()
                        .expect("drain_order only contains non-empty buffers");
                    let was_terminal = matches!(event, StreamEvent::Terminal(_));
                    let buf_empty = buf.is_empty();
                    if !buf_empty {
                        s.drain_order.push_back(sid.clone());
                    } else if was_terminal {
                        s.streams.remove(&sid);
                    }
                    return Some(match event {
                        StreamEvent::Chunk(b) => WriteWork::Chunk(b),
                        StreamEvent::Terminal(t) => WriteWork::Terminal(t),
                    });
                }
                if s.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub fn close(self: &Arc<Self>) {
        {
            let mut s = self.state.lock().expect("router mutex poisoned");
            s.closed = true;
        }
        self.notify.notify_one();
    }
}

impl ClientChannel {
    pub fn new(router: Arc<BinaryRouter>, metrics: Arc<Metrics>) -> Self {
        Self { router, metrics }
    }

    pub fn push_chunk(&self, stream_id: &StreamId, chunk: Bytes) {
        let dropped;
        {
            let mut s = self.router.state.lock().expect("router mutex poisoned");
            if s.closed {
                return;
            }
            let cap = self.router.cap_bytes;
            let buf = s
                .streams
                .entry(stream_id.clone())
                .or_insert_with(|| StreamBuffer::new(cap));
            let was_empty = buf.is_empty();
            dropped = buf.push_chunk(chunk);
            if was_empty {
                s.drain_order.push_back(stream_id.clone());
            }
        }
        if dropped > 0 {
            self.metrics.backpressure_drops_total.inc_by(dropped);
            tracing::warn!(
                stream = %stream_id,
                dropped,
                "backpressure drop: oldest chunks dropped"
            );
        }
        self.router.notify.notify_one();
    }

    pub fn push_terminal(&self, stream_id: &StreamId, json: String) {
        {
            let mut s = self.router.state.lock().expect("router mutex poisoned");
            if s.closed {
                return;
            }
            let cap = self.router.cap_bytes;
            let buf = s
                .streams
                .entry(stream_id.clone())
                .or_insert_with(|| StreamBuffer::new(cap));
            let was_empty = buf.is_empty();
            buf.push_terminal(json);
            if was_empty {
                s.drain_order.push_back(stream_id.clone());
            }
        }
        self.router.notify.notify_one();
    }

    pub fn push_text(&self, json: String) {
        {
            let mut s = self.router.state.lock().expect("router mutex poisoned");
            if s.closed {
                return;
            }
            s.text.push_back(json);
        }
        self.router.notify.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.router
            .state
            .lock()
            .expect("router mutex poisoned")
            .closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn mk() -> (Arc<BinaryRouter>, ClientChannel) {
        let m = Arc::new(Metrics::new().unwrap());
        let r = BinaryRouter::new(1024);
        let c = ClientChannel::new(r.clone(), m);
        (r, c)
    }

    #[tokio::test]
    async fn push_text_drained_immediately() {
        let (r, c) = mk();
        c.push_text("hello".into());
        match r.next_work().await.unwrap() {
            WriteWork::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[tokio::test]
    async fn push_chunk_then_drain() {
        let (r, c) = mk();
        let sid = StreamId::from("s1");
        c.push_chunk(&sid, Bytes::from_static(b"abc"));
        match r.next_work().await.unwrap() {
            WriteWork::Chunk(b) => assert_eq!(&b[..], b"abc"),
            _ => panic!("expected Chunk"),
        }
    }

    #[tokio::test]
    async fn chunks_then_terminal_in_order() {
        let (r, c) = mk();
        let sid = StreamId::from("s1");
        c.push_chunk(&sid, Bytes::from_static(b"a"));
        c.push_chunk(&sid, Bytes::from_static(b"b"));
        c.push_terminal(&sid, "done".into());
        assert!(matches!(r.next_work().await.unwrap(), WriteWork::Chunk(_)));
        assert!(matches!(r.next_work().await.unwrap(), WriteWork::Chunk(_)));
        assert!(matches!(
            r.next_work().await.unwrap(),
            WriteWork::Terminal(_)
        ));
    }

    #[tokio::test]
    async fn two_streams_round_robin() {
        let (r, c) = mk();
        let s1 = StreamId::from("s1");
        let s2 = StreamId::from("s2");
        c.push_chunk(&s1, Bytes::from_static(b"1a"));
        c.push_chunk(&s2, Bytes::from_static(b"2a"));
        c.push_chunk(&s1, Bytes::from_static(b"1b"));
        c.push_chunk(&s2, Bytes::from_static(b"2b"));

        let mut out: Vec<Vec<u8>> = Vec::new();
        for _ in 0..4 {
            let w = timeout(Duration::from_millis(50), r.next_work())
                .await
                .expect("timed out waiting for work")
                .expect("got None from next_work");
            match w {
                WriteWork::Chunk(b) => out.push(b.to_vec()),
                _ => panic!("expected Chunk"),
            }
        }
        assert_eq!(
            out,
            vec![
                b"1a".to_vec(),
                b"2a".to_vec(),
                b"1b".to_vec(),
                b"2b".to_vec()
            ]
        );
    }

    #[tokio::test]
    async fn drop_oldest_increments_metric() {
        let m = Arc::new(Metrics::new().unwrap());
        let r = BinaryRouter::new(100);
        let c = ClientChannel::new(r, m.clone());
        let sid = StreamId::from("s1");
        c.push_chunk(&sid, Bytes::from(vec![0u8; 60]));
        c.push_chunk(&sid, Bytes::from(vec![0u8; 60]));
        assert_eq!(m.backpressure_drops_total.get(), 1);
    }

    #[tokio::test]
    async fn close_returns_none_after_drain() {
        let (r, c) = mk();
        c.push_text("t".into());
        r.close();
        assert!(matches!(r.next_work().await, Some(WriteWork::Text(_))));
        assert!(r.next_work().await.is_none());
    }
}
