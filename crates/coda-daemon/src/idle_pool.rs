// SPDX-License-Identifier: AGPL-3.0-only

//! Idle backend servers kept between turns, least recently used first.

use std::sync::{Mutex, PoisonError};

use coda_protocol::types::SessionId;

/// Servers kept idle at once; the least recently used one is stopped beyond this.
/// Each is a live `codex` process, far heavier than an idle in-memory
/// conversation, so the limit is much lower than the one for model sessions.
pub(crate) const MAX_IDLE_SERVERS: usize = 8;

struct State<T> {
    items: Vec<(SessionId, T)>,
    closed: bool,
}

/// At most `max` idle values, one per session.
pub(crate) struct IdlePool<T> {
    state: Mutex<State<T>>,
    max: usize,
}

impl<T> IdlePool<T> {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            state: Mutex::new(State {
                items: Vec::new(),
                closed: false,
            }),
            max,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Removes and returns the idle value of `session`.
    pub(crate) fn take(&self, session: &SessionId) -> Option<T> {
        let mut state = self.state();
        let at = state.items.iter().position(|(id, _)| id == session)?;
        Some(state.items.remove(at).1)
    }

    /// Keeps `item` as the most recent. Returns what the caller must stop: the
    /// value pushed out by the limit, or `item` itself once the pool is closed.
    pub(crate) fn keep(&self, session: SessionId, item: T) -> Option<(SessionId, T)> {
        let mut state = self.state();
        if state.closed {
            return Some((session, item));
        }
        state.items.push((session, item));
        (state.items.len() > self.max).then(|| state.items.remove(0))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.state().items.len()
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        self.state().items.clear();
    }

    #[cfg(test)]
    pub(crate) fn holds(&self, session: &SessionId) -> bool {
        self.state().items.iter().any(|(id, _)| id == session)
    }

    /// Closes the pool and returns everything it held.
    pub(crate) fn close(&self) -> Vec<(SessionId, T)> {
        let mut state = self.state();
        state.closed = true;
        std::mem::take(&mut state.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> SessionId {
        SessionId::new(name)
    }

    #[test]
    fn keeping_past_the_limit_pushes_out_the_least_recently_used() {
        // Arrange
        let pool = IdlePool::new(2);
        pool.keep(id("a"), 1);
        pool.keep(id("b"), 2);

        // Act
        let evicted = pool.keep(id("c"), 3);

        // Assert
        assert_eq!(evicted, Some((id("a"), 1)));
        assert_eq!(pool.take(&id("a")), None);
        assert_eq!(pool.take(&id("c")), Some(3));
    }

    #[test]
    fn a_taken_value_is_no_longer_idle_and_a_reuse_counts_as_recent() {
        // Arrange
        let pool = IdlePool::new(2);
        pool.keep(id("a"), 1);
        pool.keep(id("b"), 2);

        // Act
        let taken = pool.take(&id("a")).unwrap();
        pool.keep(id("a"), taken);
        let evicted = pool.keep(id("c"), 3);

        // Assert
        assert_eq!(evicted, Some((id("b"), 2)));
    }

    #[test]
    fn a_closed_pool_hands_back_everything_it_held_and_refuses_more() {
        // Arrange
        let pool = IdlePool::new(4);
        pool.keep(id("a"), 1);

        // Act
        let held = pool.close();
        let refused = pool.keep(id("b"), 2);

        // Assert
        assert_eq!(held, vec![(id("a"), 1)]);
        assert_eq!(refused, Some((id("b"), 2)));
    }
}
