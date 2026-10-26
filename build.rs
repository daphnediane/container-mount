/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The pre-1.0 wire fingerprint: the first 8 bytes of SHA-256 over
//! `src/proto.rs` (line endings normalized), as `CONTAINER_MOUNT_WIRE`.
//! Client and server compare it in the handshake and the version stamp
//! carries it, so builds from different protocol sources refuse each
//! other clearly while `proto::VERSION` stays 0. Runs on the build host,
//! so the musl server needs no extra toolchain.

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=src/proto.rs");
    let source = std::fs::read("src/proto.rs").expect("src/proto.rs is part of the crate");
    let normalized: Vec<u8> = source.into_iter().filter(|&b| b != b'\r').collect();
    let digest = Sha256::digest(&normalized);
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=CONTAINER_MOUNT_WIRE={hex}");
}
