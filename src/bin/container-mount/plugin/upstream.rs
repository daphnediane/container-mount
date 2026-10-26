/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The `container` releases whose internals the plugin has verified.
//!
//! The plugin leans on things upstream doesn't promise: the
//! apiserver's `containerDial` XPC route (its keys, error payload, and
//! fd reply) and `container copy` directory semantics. Like
//! container-distro's `VERIFIED_CONTAINER_MINOR`, the list grows only
//! after a new minor passes the checks in
//! docs/design/mount-backends.md "Version gating" — a small duplicate
//! of container-distro's gate, kept separate on purpose.

use super::engine;

/// `container` `major.minor` versions the plugin's internals checks
/// have passed on.
pub const VERIFIED_CONTAINER_MINOR: &[&str] = &["1.6"];

/// Whether `version` (`server.version`) is a verified release.
fn verified(version: &str) -> bool {
    VERIFIED_CONTAINER_MINOR
        .iter()
        .any(|k| version == *k || version.starts_with(&format!("{k}.")))
}

/// Warn (once per process) when the `container` daemon is not a
/// verified release. Every path that dials or copies calls this first.
pub fn warn_unverified_version(dev: bool) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if let Some(v) = engine::server_version(dev)
            && !verified(&v)
        {
            eprintln!(
                "warning: `container` {v} has not been verified — `container mount` uses \
                 internal interfaces (the apiserver dial route) that may have changed"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verified_matches_minor_only() {
        assert!(verified("1.6"));
        assert!(verified("1.6.1"));
        assert!(!verified("1.60.0"));
        assert!(!verified("1.7.0"));
        assert!(!verified("1.5.2"));
    }
}
