// SPDX-License-Identifier: Apache-2.0

//! `session.set_mode`: changes the autonomy mode of an idle session.
//!
//! The mode is read when a backend process, a permission broker or a model loop
//! is built, so the change drops the ones the daemon holds and the next turn
//! builds them again under the new mode. Its stored events stay.

use std::sync::Arc;

use coda_protocol::types::{Mode, SessionId};
use coda_store::SessionRecord;
use tracing::info;

use super::{TurnError, Turns, not_found_or, remove_pidfile_of, stop};

impl Turns {
    /// Stores `mode` for `session` and returns the session after the change.
    /// Asking for the mode the session already has changes nothing and keeps
    /// what the daemon holds for it.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], and [`TurnError::Busy`] while a
    /// turn runs. A cancelled turn is waited for first, as `session.close` does.
    pub(crate) async fn set_mode(
        self: &Arc<Self>,
        session: &SessionId,
        mode: Mode,
    ) -> Result<SessionRecord, TurnError> {
        let id = session.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, session))?;
        self.await_wind_down(session).await?;
        let _hold = self.hold(&record)?;
        let id = session.clone();
        let current = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, session))?;
        if current.mode == mode {
            return Ok(current);
        }
        let id = session.clone();
        self.hub
            .with_store_mut(move |store| store.set_session_mode(&id, mode))
            .await
            .map_err(|e| not_found_or(e, session))?;
        self.brokers.forget(session);
        self.drop_idle(session).await;
        info!(session = %session, ?mode, "changed the session mode");
        let id = session.clone();
        self.hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, session))
    }

    /// Stops the pooled backend server and drops the idle model loop, which keep
    /// the mode they were built with. A model loop hands its spent budget on.
    pub(super) async fn drop_idle(&self, session: &SessionId) {
        if let Some(mut server) = self.idle.take(session) {
            let pid = server.pid();
            stop(&mut server, session).await;
            remove_pidfile_of(&self.config.pidfile_dir, session, pid);
        }
        self.models.drop_idle_loop(session);
    }
}
