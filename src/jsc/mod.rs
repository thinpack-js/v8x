//! JavaScriptCore engine backend: the v8__* C-ABI implemented over JSC.
//!
//! The port mirrors the v8 C ABI verbatim: JSC's generated `kJSType*` constants appear in
//! patterns, and the `extern "C"` entry points hand private ABI structs across a `pub(crate)`
//! boundary. Both are properties of the ported ABI, not of this crate's Rust API.
#![allow(non_snake_case, non_upper_case_globals, private_interfaces)]

pub(crate) mod arraybuffer;
pub(crate) mod cli_extra;
pub(crate) mod core;
pub(crate) mod exception;
pub(crate) mod function;
pub(crate) mod init;
pub(crate) mod inspector;
pub(crate) mod introspect;
pub(crate) mod isolate;
pub(crate) mod jsc_sys;
pub(crate) mod misc;
pub(crate) mod module;
pub(crate) mod object;
pub(crate) mod primitive;
pub(crate) mod property;
pub(crate) mod serializer;
pub(crate) mod shims;
pub(crate) mod simdutf;
pub(crate) mod string;
pub(crate) mod terminate;
pub(crate) mod value;
