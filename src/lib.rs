/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! container-mount — a small file server for Linux guests and its validating
//! host client (docs/design/protocol.md).
//!
//! `container-mount serve` runs in the guest on a socket the VM relays to the
//! host; the [`client`] and [`connect`] halves are what a host program —
//! the `container-mount` CLI itself today, an FSKit file system in the future —
//! uses to reach the guest's files. [`proto`] is the shared wire
//! protocol, [`symlink`] the confinement rules, and [`servers`] /
//! [`release`] locate and download server builds.

pub mod client;
pub mod connect;
#[cfg(target_os = "macos")]
pub mod dial;
pub mod proto;
pub mod release;
pub mod servers;
pub mod symlink;
