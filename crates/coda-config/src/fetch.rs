// SPDX-License-Identifier: Apache-2.0

//! The `[fetch]` table: which hosts the `fetch` tool may read. It is off while
//! `allow_hosts` is empty, and only the user's `config.toml` can fill it.

use std::net::IpAddr;

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const FETCH_KEY: &str = "fetch";
/// Key of the host allow list inside `[fetch]`.
pub const ALLOW_HOSTS_KEY: &str = "allow_hosts";
/// Key of the byte cap inside `[fetch]`.
pub const MAX_BYTES_KEY: &str = "max_bytes";
/// Key of the time limit inside `[fetch]`.
pub const TIMEOUT_MS_KEY: &str = "timeout_ms";

/// Bytes of a page the tool reads when `max_bytes` is unset.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// The most `max_bytes` may be.
pub const MAX_BYTES_LIMIT: usize = 1024 * 1024;
/// Milliseconds one fetch may take when `timeout_ms` is unset.
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// The most `timeout_ms` may be.
pub const FETCH_TIMEOUT_LIMIT_MS: u64 = 120_000;

/// Prefix of an allow list entry that matches the subdomains of a name.
pub const SUBDOMAIN_PREFIX: &str = "*.";
const MAX_HOST_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;

/// The `[fetch]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchConfig {
    /// Host names the tool may read, such as `docs.rs`, or `*.example.org` for the
    /// subdomains of a name. Empty, the default, leaves the tool out of every
    /// session. An address such as `10.0.0.1` is not a host name and is refused.
    pub allow_hosts: Vec<String>,
    /// The most bytes of a page that are read; the rest is cut.
    pub max_bytes: usize,
    /// How long one fetch may take, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            allow_hosts: Vec::new(),
            max_bytes: DEFAULT_MAX_BYTES,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// Why an allow list entry is refused, or `None` when it is a lowercase host
/// name with at least two labels, optionally led by `*.`.
#[must_use]
pub fn host_problem(entry: &str) -> Option<&'static str> {
    let name = entry.strip_prefix(SUBDOMAIN_PREFIX).unwrap_or(entry);
    if name.parse::<IpAddr>().is_ok() {
        return Some("an address is not a host name");
    }
    if name.is_empty() || name.len() > MAX_HOST_BYTES {
        return Some("a host name of 1 to 253 bytes");
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return Some("a name with at least two parts, such as docs.rs");
    }
    if labels.last().is_some_and(|last| reads_as_number(last)) {
        return Some("a name whose last part is not a number, which a URL reads as an address");
    }
    for label in labels {
        if label.is_empty() || label.len() > MAX_LABEL_BYTES {
            return Some("parts of 1 to 63 bytes between dots");
        }
        let plain = label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !plain || label.starts_with('-') || label.ends_with('-') {
            return Some("lowercase letters, digits and inner hyphens only");
        }
    }
    None
}

/// Whether a URL parser takes a last label for the end of an IPv4 address: all
/// digits, or `0x` and hex digits.
fn reads_as_number(label: &str) -> bool {
    let hex = label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"));
    label.bytes().all(|b| b.is_ascii_digit())
        || hex.is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_and_subdomain_patterns_are_accepted() {
        for entry in [
            "docs.rs",
            "*.rust-lang.org",
            "a1.b2.example.org",
            "xn--e1afmkfd.xn--p1ai",
        ] {
            assert_eq!(host_problem(entry), None, "{entry}");
        }
    }

    #[test]
    fn addresses_schemes_paths_ports_and_odd_names_are_refused() {
        for entry in [
            "10.0.0.1",
            "127.1",
            "0x7f.1",
            "a.0x7f",
            "1.2.3.4.5",
            "docs.rs.1",
            "::1",
            "[::1]",
            "https://docs.rs",
            "docs.rs/path",
            "docs.rs:443",
            "user@docs.rs",
            "localhost",
            "Docs.rs",
            "docs..rs",
            ".docs.rs",
            "docs.rs.",
            "-a.docs.rs",
            "a-.docs.rs",
            "*",
            "*.",
            "*docs.rs",
            "doc s.rs",
            "",
        ] {
            assert!(host_problem(entry).is_some(), "{entry:?}");
        }
    }

    #[test]
    fn the_defaults_leave_the_tool_off() {
        // Act
        let config = FetchConfig::default();

        // Assert
        assert_eq!(config.allow_hosts, Vec::<String>::new());
        assert_eq!(config.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(config.timeout_ms, DEFAULT_TIMEOUT_MS);
    }
}
