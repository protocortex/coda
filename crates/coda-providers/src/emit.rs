// SPDX-License-Identifier: Apache-2.0

//! Where a backend adapter sends the events of a turn.

use std::future::Future;

use coda_protocol::events::Event;

/// Takes the events an adapter produces.
///
/// An implementation may wait, so a consumer that falls behind slows the adapter
/// instead of letting events pile up. Any `FnMut(Event)` is a sink that never waits.
pub trait Emit: Send {
    /// Takes one event.
    fn emit(&mut self, event: Event) -> impl Future<Output = ()> + Send;
}

impl<F: FnMut(Event) + Send> Emit for F {
    fn emit(&mut self, event: Event) -> impl Future<Output = ()> + Send {
        self(event);
        std::future::ready(())
    }
}
