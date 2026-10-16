// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::{HashSet, VecDeque};

use coda_protocol::types::RequestId;

/// Most permission request ids remembered per subscription.
pub(crate) const SEEN_REQUEST_CAPACITY: usize = 1024;

/// A bounded set of permission request ids that forgets the oldest first.
#[derive(Debug, Clone)]
pub(crate) struct SeenRequests {
    capacity: usize,
    order: VecDeque<RequestId>,
    ids: HashSet<RequestId>,
}

impl SeenRequests {
    pub(crate) fn new() -> Self {
        Self::with_capacity(SEEN_REQUEST_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            ids: HashSet::with_capacity(capacity),
        }
    }

    /// Remembers `id`; returns whether it was not remembered before.
    pub(crate) fn insert(&mut self, id: RequestId) -> bool {
        if !self.ids.insert(id.clone()) {
            return false;
        }
        if self.order.len() == self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.ids.remove(&oldest);
        }
        self.order.push_back(id);
        true
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::RequestId;

    use super::SeenRequests;

    #[test]
    fn oldest_id_is_evicted_once_capacity_is_exceeded() {
        // Arrange
        let mut seen = SeenRequests::with_capacity(2);
        let id = |s: &str| RequestId::new(s);
        assert!(seen.insert(id("a")));
        assert!(seen.insert(id("b")));

        // Act
        let third = seen.insert(id("c"));

        // Assert
        assert!(third);
        assert!(seen.insert(id("a")), "a was evicted, so it is new again");
        assert!(!seen.insert(id("c")), "c is still remembered");
    }

    #[test]
    fn repeated_id_is_reported_as_seen() {
        // Arrange
        let mut seen = SeenRequests::with_capacity(2);
        seen.insert(RequestId::new("a"));

        // Act
        let again = seen.insert(RequestId::new("a"));

        // Assert
        assert!(!again);
    }
}
