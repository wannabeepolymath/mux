use std::collections::VecDeque;

use crate::types::StreamId;

/// A bounded FIFO queue for pending TTS requests.
#[derive(Debug)]
pub struct BoundedQueue<T> {
    inner: VecDeque<T>,
    max_depth: usize,
}

impl<T> BoundedQueue<T> {
    pub fn new(max_depth: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(max_depth),
            max_depth,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_full(&self) -> bool {
        self.inner.len() >= self.max_depth
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// Add to the back of the queue. Returns Err if full.
    pub fn push_back(&mut self, item: T) -> Result<(), T> {
        if self.is_full() {
            return Err(item);
        }
        self.inner.push_back(item);
        Ok(())
    }

    /// Add to the front of the queue (for priority retries). Returns Err if full.
    pub fn push_front(&mut self, item: T) -> Result<(), T> {
        if self.is_full() {
            return Err(item);
        }
        self.inner.push_front(item);
        Ok(())
    }

    /// Peek at the next item without removing.
    pub fn front(&self) -> Option<&T> {
        self.inner.front()
    }

    /// Peek at an item by index without removing.
    pub fn peek_at(&self, idx: usize) -> Option<&T> {
        self.inner.get(idx)
    }

    /// Remove and return the next item.
    pub fn pop_front(&mut self) -> Option<T> {
        self.inner.pop_front()
    }

    /// Remove and return the item at the given index.
    pub fn remove_at(&mut self, idx: usize) -> Option<T> {
        self.inner.remove(idx)
    }
}

impl<T: HasStreamId> BoundedQueue<T> {
    /// Remove an item by stream_id (for cancellation). Returns the item if found.
    pub fn remove_by_stream_id(&mut self, stream_id: &StreamId) -> Option<T> {
        let pos = self
            .inner
            .iter()
            .position(|item| item.stream_id() == stream_id)?;
        self.inner.remove(pos)
    }
}

/// Trait for items that carry a stream_id, enabling removal by ID.
pub trait HasStreamId {
    fn stream_id(&self) -> &StreamId;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestItem {
        id: StreamId,
    }
    impl HasStreamId for TestItem {
        fn stream_id(&self) -> &StreamId {
            &self.id
        }
    }

    #[test]
    fn basic_fifo() {
        let mut q = BoundedQueue::new(3);
        q.push_back(1).unwrap();
        q.push_back(2).unwrap();
        q.push_back(3).unwrap();
        assert_eq!(q.pop_front(), Some(1));
        assert_eq!(q.pop_front(), Some(2));
        assert_eq!(q.pop_front(), Some(3));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn rejects_when_full() {
        let mut q = BoundedQueue::new(2);
        q.push_back(1).unwrap();
        q.push_back(2).unwrap();
        assert!(q.push_back(3).is_err());
    }

    #[test]
    fn push_front_for_retry() {
        let mut q = BoundedQueue::new(3);
        q.push_back(2).unwrap();
        q.push_back(3).unwrap();
        q.push_front(1).unwrap();
        assert_eq!(q.pop_front(), Some(1));
    }

    #[test]
    fn remove_by_stream_id() {
        let mut q: BoundedQueue<TestItem> = BoundedQueue::new(5);
        q.push_back(TestItem {
            id: "a".into(),
        })
        .unwrap();
        q.push_back(TestItem {
            id: "b".into(),
        })
        .unwrap();
        q.push_back(TestItem {
            id: "c".into(),
        })
        .unwrap();

        let removed = q.remove_by_stream_id(&StreamId::from("b"));
        assert!(removed.is_some());
        assert_eq!(q.len(), 2);
    }
}
