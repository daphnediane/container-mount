/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Dial a vsock port inside a running Apple `container` guest.
//!
//! `container-apiserver`'s `containerDial` XPC route hands back a
//! connected fd to a guest vsock port — the route `container build`
//! uses to reach buildkit. It works on any running container with no
//! create-time flag (docs/design/mount-backends.md). The connection
//! is pinned to the apiserver's code signature before anything is
//! sent, so a look-alike Mach service can't hand us a stream.
//!
//! Raw libxpc: the route has no CLI verb, and the C API is all this
//! needs — one dictionary out, one dictionary (an fd or an error) back.

// cspell:ignore UPBK2H6LZM

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd};

use anyhow::{Context, Result, bail};

/// The apiserver's Mach service.
pub const APISERVER_SERVICE: &str = "com.apple.container.apiserver";

/// Code requirement the apiserver must satisfy: Apple's signed release
/// build, by identifier and team.
pub const APISERVER_REQUIREMENT: &str = "identifier \"com.apple.container.apiserver\" \
     and anchor apple generic and certificate leaf[subject.OU] = \"UPBK2H6LZM\"";

const ROUTE_KEY: &CStr = c"com.apple.container.xpc.route";
const ERROR_KEY: &CStr = c"com.apple.container.xpc.error";
const ROUTE_DIAL: &CStr = c"containerDial";

type XpcObject = *mut c_void;

#[repr(C)]
struct Opaque {
    _p: [u8; 0],
}

// libxpc and the blocks runtime are both in libSystem.
unsafe extern "C" {
    static _xpc_type_dictionary: Opaque;
    static _xpc_type_error: Opaque;
    static _xpc_error_key_description: c_char;
    static _NSConcreteGlobalBlock: Opaque;

    fn xpc_connection_create_mach_service(
        name: *const c_char,
        queue: *mut c_void,
        flags: u64,
    ) -> XpcObject;
    fn xpc_connection_set_event_handler(conn: XpcObject, handler: *const c_void);
    fn xpc_connection_set_peer_code_signing_requirement(
        conn: XpcObject,
        requirement: *const c_char,
    ) -> c_int;
    fn xpc_connection_activate(conn: XpcObject);
    fn xpc_connection_cancel(conn: XpcObject);
    fn xpc_connection_send_message_with_reply_sync(conn: XpcObject, msg: XpcObject) -> XpcObject;
    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const XpcObject,
        count: usize,
    ) -> XpcObject;
    fn xpc_dictionary_set_string(dict: XpcObject, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_set_uint64(dict: XpcObject, key: *const c_char, value: u64);
    fn xpc_dictionary_get_string(dict: XpcObject, key: *const c_char) -> *const c_char;
    fn xpc_dictionary_get_data(
        dict: XpcObject,
        key: *const c_char,
        len: *mut usize,
    ) -> *const c_void;
    fn xpc_dictionary_dup_fd(dict: XpcObject, key: *const c_char) -> c_int;
    fn xpc_get_type(obj: XpcObject) -> *const Opaque;
    fn xpc_release(obj: XpcObject);
}

/// A capture-free global block, laid out per the blocks ABI — what
/// `^(xpc_object_t e) {}` compiles to. libxpc requires an event
/// handler before activation; ours ignores events, since the sync send
/// reports a dead connection as its reply.
#[repr(C)]
struct Block {
    isa: *const Opaque,
    flags: c_int,
    reserved: c_int,
    invoke: unsafe extern "C" fn(*const Block, XpcObject),
    descriptor: *const BlockDescriptor,
}

#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
}

// SAFETY: the block is immutable and capture-free.
unsafe impl Sync for Block {}

const BLOCK_IS_GLOBAL: c_int = 1 << 28;

unsafe extern "C" fn ignore_event(_block: *const Block, _event: XpcObject) {}

static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<Block>(),
};

static IGNORE_EVENTS: Block = Block {
    isa: &raw const _NSConcreteGlobalBlock,
    flags: BLOCK_IS_GLOBAL,
    reserved: 0,
    invoke: ignore_event,
    descriptor: &DESCRIPTOR,
};

/// Releases an XPC object on drop.
struct Xpc(XpcObject);

impl Drop for Xpc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference to a live XPC object.
            unsafe { xpc_release(self.0) };
        }
    }
}

/// Cancels and releases a connection on drop.
struct Connection(Xpc);

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: the connection is live until `Xpc` releases it.
        unsafe { xpc_connection_cancel(self.0.0) };
    }
}

/// Dial vsock `port` in running container `id` and return the
/// connected stream. `requirement` is the code requirement the
/// apiserver must meet ([`APISERVER_REQUIREMENT`] in production).
pub fn dial(id: &str, port: u32, requirement: &str) -> Result<OwnedFd> {
    let id = CString::new(id).context("container id contains NUL")?;
    let service = CString::new(APISERVER_SERVICE)?;
    let requirement = CString::new(requirement).context("requirement contains NUL")?;
    // SAFETY: every pointer passed is a valid NUL-terminated string or
    // a live XPC object, objects are released by `Xpc`, and the reply's
    // type is checked before any dictionary accessor touches it.
    unsafe {
        let conn = Connection(Xpc(xpc_connection_create_mach_service(
            service.as_ptr(),
            std::ptr::null_mut(),
            0,
        )));
        if conn.0.0.is_null() {
            bail!("couldn't create an XPC connection to {APISERVER_SERVICE}");
        }
        xpc_connection_set_event_handler(conn.0.0, (&raw const IGNORE_EVENTS).cast());
        if xpc_connection_set_peer_code_signing_requirement(conn.0.0, requirement.as_ptr()) != 0 {
            bail!("couldn't set the apiserver code-signing requirement");
        }
        xpc_connection_activate(conn.0.0);

        let msg = Xpc(xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0));
        xpc_dictionary_set_string(msg.0, ROUTE_KEY.as_ptr(), ROUTE_DIAL.as_ptr());
        xpc_dictionary_set_string(msg.0, c"id".as_ptr(), id.as_ptr());
        xpc_dictionary_set_uint64(msg.0, c"port".as_ptr(), port.into());
        let reply = Xpc(xpc_connection_send_message_with_reply_sync(conn.0.0, msg.0));

        let kind = xpc_get_type(reply.0);
        if kind == &raw const _xpc_type_error {
            let text = xpc_dictionary_get_string(reply.0, &raw const _xpc_error_key_description);
            let text = if text.is_null() {
                "unknown error".into()
            } else {
                CStr::from_ptr(text).to_string_lossy()
            };
            // A failed peer requirement also lands here.
            bail!(
                "{APISERVER_SERVICE} unavailable or failed its signature check ({text}) \
                 — is `container system start` running?"
            );
        }
        if kind != &raw const _xpc_type_dictionary {
            bail!("unexpected XPC reply type from {APISERVER_SERVICE}");
        }
        let mut len = 0usize;
        let data = xpc_dictionary_get_data(reply.0, ERROR_KEY.as_ptr(), &mut len);
        if !data.is_null() {
            let raw = std::slice::from_raw_parts(data.cast::<u8>(), len);
            bail!("dial failed: {}", error_message(raw));
        }
        let fd = xpc_dictionary_dup_fd(reply.0, c"fd".as_ptr());
        if fd < 0 {
            bail!("dial reply carried no fd");
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// The `message` of the apiserver's JSON error payload
/// (`{"code": …, "message": …}`), or the raw text if it isn't one.
fn error_message(raw: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_message_reads_json_payload() {
        let raw = br#"{"code":"notFound","message":"container x not found"}"#;
        assert_eq!(error_message(raw), "container x not found");
        assert_eq!(error_message(b"plain"), "plain");
    }

    /// Needs `container system start`: a requirement the real apiserver
    /// can't meet must fail before any dial happens.
    #[test]
    #[ignore]
    fn test_dial_refuses_unpinned_apiserver() {
        let err = dial("no-such-container", 900, "identifier \"not.the.apiserver\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("signature check"), "{err}");
        let err = dial("no-such-container", 900, APISERVER_REQUIREMENT)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("dial failed"), "{err}");
    }

    #[test]
    fn test_global_block_layout() {
        // flags + reserved pack into one word after `isa`.
        assert_eq!(
            std::mem::size_of::<Block>(),
            4 * std::mem::size_of::<usize>()
        );
    }
}
