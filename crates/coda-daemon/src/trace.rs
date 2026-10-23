// SPDX-License-Identifier: AGPL-3.0-only

//! Spans the daemon opens around requests and sessions.
//!
//! Spans carry identifiers only, never message content.

use coda_protocol::types::{SessionId, TurnId};
use coda_telemetry::attrs::{CODA_BACKEND_KIND, CODA_SESSION_ID, CODA_TURN_ID};
use tracing::{Span, info_span};

/// Name of the span that wraps one request.
pub const REQUEST_SPAN: &str = "coda.request";
/// Name of the span that wraps work on one session.
pub const SESSION_SPAN: &str = "coda.session";
/// Name of the span that wraps one backend child process.
pub const BACKEND_SPAN: &str = "coda.backend.process";
/// Attribute holding the JSON-RPC method of a request.
pub const RPC_METHOD: &str = "rpc.method";

/// Opens the span for a request to `method`.
#[must_use]
pub fn request_span(method: &str) -> Span {
    info_span!(REQUEST_SPAN, { RPC_METHOD } = method)
}

/// Opens the span for work on `session`.
#[must_use]
pub fn session_span(session: &SessionId) -> Span {
    info_span!(SESSION_SPAN, { CODA_SESSION_ID } = %session)
}

/// Opens the span for the backend process that runs `turn`.
#[must_use]
pub fn backend_span(session: &SessionId, turn: &TurnId, backend: &str) -> Span {
    info_span!(
        BACKEND_SPAN,
        { CODA_SESSION_ID } = %session,
        { CODA_TURN_ID } = %turn,
        { CODA_BACKEND_KIND } = backend,
    )
}

#[cfg(test)]
pub(crate) mod capture {
    use std::collections::BTreeMap;
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};

    use tracing::field::{Field, Visit};
    use tracing::span::Attributes;
    use tracing::subscriber::DefaultGuard;
    use tracing::{Event, Id, Level, Subscriber};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    /// A span as it was created: its name and recorded fields.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct SpanRecord {
        pub name: String,
        pub fields: BTreeMap<String, String>,
    }

    /// A log event as it was emitted: its level and recorded fields, the
    /// message among them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct EventRecord {
        pub level: Level,
        pub fields: BTreeMap<String, String>,
    }

    /// Spans and log events created while the guard returned by [`install`]
    /// lives on this thread.
    #[derive(Clone, Default)]
    pub(crate) struct Spans {
        spans: Arc<Mutex<Vec<SpanRecord>>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
    }

    impl Spans {
        pub(crate) fn all(&self) -> Vec<SpanRecord> {
            self.spans
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// The log events of `level`, oldest first.
        pub(crate) fn events_at(&self, level: Level) -> Vec<EventRecord> {
            let events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
            events
                .iter()
                .filter(|e| e.level == level)
                .cloned()
                .collect()
        }

        pub(crate) fn named(&self, name: &str) -> Vec<SpanRecord> {
            self.all().into_iter().filter(|s| s.name == name).collect()
        }
    }

    #[derive(Default)]
    struct Fields(BTreeMap<String, String>);

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S: Subscriber> Layer<S> for Spans {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            self.spans
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(SpanRecord {
                    name: attrs.metadata().name().to_owned(),
                    fields: fields.0,
                });
        }

        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(EventRecord {
                    level: *event.metadata().level(),
                    fields: fields.0,
                });
        }
    }

    /// Records spans on the current thread until the guard drops. Use a
    /// current-thread runtime so the code under test stays on this thread.
    pub(crate) fn install() -> (Spans, DefaultGuard) {
        // A callsite first hit on a thread with no subscriber can cache "never
        // interested" while other threads swap scoped subscribers in and out. An
        // always-interested global registry keeps every callsite enabled.
        static GLOBAL: OnceLock<()> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let _ = tracing::subscriber::set_global_default(Registry::default());
        });
        let spans = Spans::default();
        let guard = tracing::subscriber::set_default(Registry::default().with(spans.clone()));
        // With a single live subscriber, tracing rebuilds callsite interest from
        // the calling thread's default alone, so a parallel test hitting a span
        // with no subscriber caches it as disabled for the whole process. A
        // process-wide subscriber keeps every callsite enabled.
        let _ = tracing::subscriber::set_global_default(Registry::default());
        tracing::callsite::rebuild_interest_cache();
        (spans, guard)
    }
}

#[cfg(test)]
mod tests {
    use coda_protocol::types::SessionId;

    use super::capture::install;
    use super::*;

    #[test]
    fn request_span_carries_the_method_name() {
        // Arrange
        let (spans, _guard) = install();

        // Act
        let _span = request_span("daemon.ping");

        // Assert
        let recorded = spans.named(REQUEST_SPAN);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].fields["rpc.method"], "daemon.ping");
    }

    #[test]
    fn session_span_carries_the_session_id() {
        // Arrange
        let (spans, _guard) = install();

        // Act
        let _span = session_span(&SessionId::new("s-7"));

        // Assert
        let recorded = spans.named(SESSION_SPAN);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].fields["coda.session.id"], "s-7");
    }
}
