use bytes::Bytes;
use std::collections::VecDeque;

#[derive(Debug)]
pub enum StreamEvent {
    Chunk(Bytes),
    Terminal(String),
}

#[derive(Debug)]
pub struct StreamBuffer {
    queue: VecDeque<StreamEvent>,
    bytes: usize,
    cap_bytes: usize,
    drops_since_log: u64,
    total_drops: u64,
}

impl StreamBuffer {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            bytes: 0,
            cap_bytes,
            drops_since_log: 0,
            total_drops: 0,
        }
    }

    /// Push a chunk; drops oldest `Chunk` events until the new chunk fits
    /// (or until no Chunks remain, in which case the new chunk is admitted
    /// even if it exceeds cap — we don't lose data we promised to send).
    /// Returns the number of chunks dropped.
    pub fn push_chunk(&mut self, chunk: Bytes) -> u64 {
        let mut dropped = 0u64;
        while self.bytes + chunk.len() > self.cap_bytes {
            let drop_idx = self
                .queue
                .iter()
                .position(|e| matches!(e, StreamEvent::Chunk(_)));
            let Some(idx) = drop_idx else { break };
            if let Some(StreamEvent::Chunk(old)) = self.queue.remove(idx) {
                self.bytes -= old.len();
                dropped += 1;
            }
        }
        self.bytes += chunk.len();
        self.queue.push_back(StreamEvent::Chunk(chunk));
        self.drops_since_log = self.drops_since_log.saturating_add(dropped);
        self.total_drops = self.total_drops.saturating_add(dropped);
        dropped
    }

    pub fn push_terminal(&mut self, json: String) {
        self.queue.push_back(StreamEvent::Terminal(json));
    }

    pub fn pop(&mut self) -> Option<StreamEvent> {
        let event = self.queue.pop_front()?;
        if let StreamEvent::Chunk(b) = &event {
            self.bytes -= b.len();
        }
        Some(event)
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn take_and_reset_drop_count(&mut self) -> u64 {
        let n = self.drops_since_log;
        self.drops_since_log = 0;
        n
    }

    pub fn total_drops(&self) -> u64 {
        self.total_drops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn empty_buffer() {
        let b = StreamBuffer::new(1024);
        assert!(b.is_empty());
        assert_eq!(b.bytes(), 0);
    }

    #[test]
    fn push_chunk_under_cap_no_drop() {
        let mut b = StreamBuffer::new(1024);
        assert_eq!(b.push_chunk(chunk(100)), 0);
        assert_eq!(b.push_chunk(chunk(200)), 0);
        assert_eq!(b.bytes(), 300);
    }

    #[test]
    fn push_chunk_over_cap_drops_oldest() {
        let mut b = StreamBuffer::new(300);
        assert_eq!(b.push_chunk(chunk(100)), 0);
        assert_eq!(b.push_chunk(chunk(100)), 0);
        assert_eq!(b.push_chunk(chunk(100)), 0);
        assert_eq!(b.push_chunk(chunk(100)), 1);
        assert_eq!(b.bytes(), 300);
        assert_eq!(b.total_drops(), 1);
    }

    #[test]
    fn push_chunk_drops_multiple_to_fit_large_chunk() {
        let mut b = StreamBuffer::new(300);
        for _ in 0..6 {
            b.push_chunk(chunk(50));
        }
        let dropped = b.push_chunk(chunk(200));
        assert_eq!(dropped, 4);
        assert_eq!(b.bytes(), 100 + 200);
    }

    #[test]
    fn push_chunk_larger_than_cap_admitted_alone() {
        let mut b = StreamBuffer::new(100);
        b.push_chunk(chunk(50));
        let dropped = b.push_chunk(chunk(500));
        assert_eq!(dropped, 1);
        assert_eq!(b.bytes(), 500);
    }

    #[test]
    fn terminal_never_dropped_and_not_counted() {
        let mut b = StreamBuffer::new(300);
        b.push_chunk(chunk(100));
        b.push_terminal(r#"{"type":"done"}"#.into());
        b.push_chunk(chunk(100));
        b.push_chunk(chunk(100));
        b.push_chunk(chunk(100));
        let mut saw_terminal = false;
        let mut chunk_count = 0;
        while let Some(e) = b.pop() {
            match e {
                StreamEvent::Terminal(_) => saw_terminal = true,
                StreamEvent::Chunk(_) => chunk_count += 1,
            }
        }
        assert!(saw_terminal, "terminal must survive chunk overflow");
        assert_eq!(chunk_count, 3, "exactly 3 chunks fit in 300 bytes");
    }

    #[test]
    fn fifo_ordering_within_stream() {
        let mut b = StreamBuffer::new(1024);
        b.push_chunk(Bytes::from(vec![1u8; 1]));
        b.push_chunk(Bytes::from(vec![2u8; 1]));
        b.push_terminal("done".into());
        match b.pop() {
            Some(StreamEvent::Chunk(c)) => assert_eq!(c[0], 1),
            _ => panic!("expected Chunk(1)"),
        }
        match b.pop() {
            Some(StreamEvent::Chunk(c)) => assert_eq!(c[0], 2),
            _ => panic!("expected Chunk(2)"),
        }
        match b.pop() {
            Some(StreamEvent::Terminal(s)) => assert_eq!(s, "done"),
            _ => panic!("expected Terminal"),
        }
        assert!(b.pop().is_none());
    }

    #[test]
    fn drop_count_is_resettable() {
        let mut b = StreamBuffer::new(100);
        b.push_chunk(chunk(60));
        b.push_chunk(chunk(60));
        assert_eq!(b.take_and_reset_drop_count(), 1);
        assert_eq!(b.take_and_reset_drop_count(), 0);
        assert_eq!(b.total_drops(), 1);
    }
}
