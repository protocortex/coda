// SPDX-License-Identifier: Apache-2.0

//! Tracing and OpenTelemetry setup, plus `GenAI` attribute names.

pub mod attrs;
pub mod init;

pub use init::{Built, TelemetryConfig, TelemetryError, TelemetryGuard, build_subscriber, init};
