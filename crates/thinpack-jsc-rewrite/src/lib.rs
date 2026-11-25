//! ES module rewriting for the system-JavaScriptCore backend.
//!
//! The macOS system framework exposes no module loader through its C API, so the
//! backend lowers ES module syntax into a function body: imports read from a
//! namespace registry, exports assign into the module's namespace object, and
//! `import.meta` becomes a placeholder. The vendored `JavaScriptCore` build links the
//! real `JSModuleRecord` and does not use this rewrite.
//!
//! The rewrite lives in its own crate so it can be unit-tested on every
//! platform, and so its lexical scanner is shared by every pass instead of each
//! one scanning the source text on its own.

#![expect(
  clippy::format_push_string,
  reason = "the generated prelude reads better as format! than as write! on an infallible String"
)]

mod lexer;
mod rewrite;

#[cfg(test)]
mod tests;

pub use rewrite::{
  RewrittenModule, has_top_level_await, import_offsets,
  rewrite_dynamic_import_calls, rewrite_es_module,
};
