// SPDX-License-Identifier: AGPL-3.0-only

//! The seam between a backend adapter and whoever answers permission requests.

use std::future::Future;

use coda_protocol::types::PermissionDecision;

use crate::tool_request::ToolRequest;

/// Answers the permission requests a backend raises.
///
/// An implementation applies [`decide`](crate::policy::decide) and, on a
/// prompt verdict, waits for a client. It resolves to
/// [`PermissionDecision::Deny`] when nobody answers in time.
pub trait PermissionGate: Send + Sync {
    /// Decides one request.
    fn request(&self, request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send;

    /// The backend wrote something, whether or not it became an event; it is alive.
    fn progress(&self) {}

    /// The backend reports it began running tool `id` on its own, with no ask.
    fn tool_started(&self, _id: &str) {}

    /// The backend reports tool `id` finished.
    fn tool_finished(&self, _id: &str) {}
}
