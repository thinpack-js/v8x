# Thinpack's v8x branch

This branch is `thinpack-js/v8x`, a fork of `denoland/v8x` at
`803a21ce38e1b8f3d015cc9c4546d56280e9681f` (`v8x 149.4.0-rc.4`), with the patches thinpack
needs. Thinpack consumes it as one pinned git source that redirects the crates.io release:

```toml
[patch.crates-io]
v8x = { git = "https://github.com/thinpack-js/v8x", rev = "<branch commit>" }
```

The published crate ships the QuickJS sources already patched. A git checkout does not, so
`build.rs` applies `patches/<engine>-NN-*.patch` onto the pinned submodule (see the
`.git`-absent guard in `setup_vendor`); the crates.io package skips that step because it has
no git metadata. Nothing outside `patches/quickjs-103-thinpack-startup.patch`,
`src/jsc/`, `crates/thinpack-jsc-rewrite/` and the dependency on that crate differs from
upstream. The submodule gitlinks are upstream's:

- `vendor/quickjs-ng` `034f2aba47ab6e7aacd869af7aa796b110d0ea2e` (MIT)
- `vendor/rusty_v8` `dd1b4e9c743b7a11dbeb99d4d0dc55979218b905`
- `vendor/wamr` `26756a5c5846c262c9e5c89f3fc8e1e693ee2539`, `vendor/webkit`
  `0f307e93cf9eb941494046a829baf7dcb15f67c9`

The QuickJS build still prepares only the trees it compiles — rusty_v8's ICU data and WAMR —
and never the unrelated engines. `vendor/webkit` is left to `tools/setup_webkit.sh`, which
only the `vendor_jsc` backend calls, exactly as upstream.

## Startup patch: `patches/quickjs-103-thinpack-startup.patch`

Applied on top of upstream's 92 QuickJS patches (`quickjs-01` … `quickjs-102`), which also
makes the file the single record of the change: the crates.io package bakes it into
`vendor/quickjs-ng/quickjs.c`, the git checkout applies it at build time. Two changes:

### O(1) token columns (upstream backport)

`js_parse_postfix_expr` derived the column of an identifier (and of a property name) by
walking back from the token to the previous newline. On minified bundles with
multi-hundred-kilobyte lines this makes cold parsing quadratic: a 1.9 MB Kubernetes client
bundle took 6.1 s to parse and an 11.65 MB bundle takes minutes. The same scan also preceded
metadata for every identifier, so it dominated startup on all large minified inputs.

The fix is upstream quickjs-ng commit
`0e194369a8650487c981e6128079ebac249a0769` ("perf(parser): Avoid quadratic identifier column
lookup"): track the current line start in `JSParseState` (and the token's line start in
`JSToken`), update it at every newline site, save/restore it across `peek_token`, and use it
for identifier and error columns. The two remaining scans in this older quickjs-ng snapshot
(`js_parse_postfix_expr` property names and `js_parse_error`) use the same tracked pointer.

Measured with a C harness on the 1.9 MB bundle: 6070 ms before, 58 ms after.
`packed::minified-cold-startup` is the regression guard.

### Hash-table lookups for large scopes

Custom change, now matching the quickjs-ng follow-up to issue #1954
(`ce30d9133105729377cd7a70b861ddfb8fcfe2e3`, "Key variable lookups by name and scope").

`find_var_in_child_scope`, `find_lexical_decl`, and the local-scope loop in
`resolve_scope_var` scanned the variable and scope chains repeatedly while parsing.
Functions with tens of thousands of declarations therefore took quadratic time even when
every line was short. The change reuses the existing `vars_htab` for these lookups and keys
each entry by name and scope level (`hash32(name ^ hash32(scope_level))`), so equal names in
different scopes land in different probe chains. `find_scoped_var_htab` walks the parent
links, uses each scope's `first` link to skip empty scopes and to jump to the scope that owns
the chain head, and compares both the name and the scope level. Declaration helpers set
`scope_level` before the entry is inserted. `with` scopes and argument scopes keep the chain
walk, because their shadowing rules depend on declaration order and on the argument-scope
sentinel.

The previous version hashed only the name, so repeated declarations filled one probe chain
and every lookup walked it; it also visited every parent scope even when a scope had no
declarations of its own. Both effects are gone. `packed::large-lexical-scope-startup` still
parses 50,000 declarations in about 11 ms instead of 9.95 s. Repeated names no longer
degenerate: 16,000 sibling blocks each declaring `let x` compile in 19 ms instead of 5.75 s,
and 16,000 catch bindings in 19 ms instead of 750 ms. Ordinary minified bundles are unchanged
(930 KB lucide: 20 ms; 3.5 MB tabler: 82 ms before, 79 ms after).

## JSC shim changes (`src/jsc`)

The macOS system-framework backend is a v8 C ABI implemented over JavaScriptCore. Thinpack
ships it as the `system_jsc` path; the vendored `vendor_jsc` build keeps upstream's native
module API and only receives the ABI-level fixes below.

### Warning-free shim

The JSC port mirrors the v8 C ABI: JSC's generated `kJSType*` constants appear in `match`
patterns and the `extern "C"` entry points hand private ABI structs across a `pub(crate)`
boundary. `src/jsc/mod.rs` allows `non_upper_case_globals` and `private_interfaces` for the
whole module, and `src/jsc/init.rs` allows `unused_mut` on the system-framework build, which
pushes no options. Without these, a `build-warnings = deny` environment (CI) fails on the
macOS JSC job. No runtime behavior changes.

### Undefined module-request specifiers

`v8__ModuleRequest__GetSpecifier` returned a null pointer when JSC left the specifier
undefined (for example, a NUL in the import text). rusty_v8's `ModuleRequest::get_specifier`
unwraps that pointer and aborts. Return the undefined value instead, as the QuickJS adapter
does, so deno_core reports "the engine did not provide a string" rather than panicking.

### Own-only property enumeration

JSC's public C API has no own-only enumeration: `JSObjectCopyPropertyNames` returns
enumerable names **including inherited ones**, so `Object.keys`, `util.inspect` and
`assert.deepStrictEqual` saw every `Buffer.prototype` method as a property of a Buffer on the
JSC backend. `GetOwnPropertyNames` now collects the own names through
`Object.getOwnPropertyNames` / `getOwnPropertySymbols` and applies `PropertyFilter` with
`getOwnPropertyDescriptor` and `propertyIsEnumerable`; `GetPropertyNames` uses that path for
`OwnOnly` and keeps the chain walk for `IncludePrototypes`. `jsstr_is_array_index` also reads
JSC's 8-bit strings, which have no UTF-16 buffer, so `SkipIndices` works on ASCII index names.

### UTF-8 writes use UTF-16 units and honor the flag

`JSStringGetUTF8CString` truncates a string at an unpaired surrogate, so a `Buffer.from` /
`Buffer#write` of text containing one lost the tail. Encode from the UTF-16 units instead,
replacing unpaired surrogates with U+FFFD (the USV semantics `kReplaceInvalidUtf8` asks for)
and stopping before a character that does not fit. `WriteUtf8` also wrote a NUL
unconditionally; V8 only does that with `WriteFlags::kNullTerminate` (bit 1), so an
exact-size write overran its slice.

### Top-level await inside a block

`has_top_level_await` only accepted `await` at brace depth 0, so a module with `await` inside
a top-level `try`/`if`/loop was wrapped in a synchronous function and failed to parse. Before
building the wrapper, run `JSCheckScriptSyntax` on the synchronous body; a module that does
not parse synchronously is wrapped in an async function. This also removes the async method
false positives the depth check was guarding against. The scanner now tracks expression-bodied
arrows too, so `const f = async () => await x` is not mistaken for top-level await.

### ESM link checks and import locations

The rewriter turns imports into destructuring, which silently yields `undefined` for a
missing export. `rewrite_es_module` now records (specifier, imported name, line) for every
named/default import and named re-export; `InstantiateModule` checks those against the
dependency's export list before evaluating it, and rejects with the V8 message and a real
stack pointing at the importer's import line. `export *` dependencies are exempt because the
rewriter cannot enumerate their names. The reported line comes from the specifier token's own
source offset, so a comment or an earlier identical import cannot move it. The original line
starts are also stored so `SourceOffsetToLocation` reports the offsets deno uses in import
errors, and an `Errored` module can be re-instantiated (the dynamic-import loop imports the
failing graph twice).

### Hoisted declarations after a statement

A logical line like `globalThis.seen = true; export const present = 1;` began with a
statement, so the line-oriented rewriter never recognized the export and left the keyword
inside the generated function. Split at `;` when the remainder starts with `import`/`export`,
then process the declaration normally.

### Evaluation errors become rejections once

A throwing synthetic evaluation step (a CommonJS module with a TDZ error, for one) leaves a
pending exception. `Evaluate` turns it into a rejected promise so the importer or `import()`
sees the rejection. JSC reports an unhandled rejection at rejection time, so the promise is
created pending, its tracking handlers are attached, and only then is it rejected; otherwise
the same CJS error would also be dispatched as uncaught. Source modules keep the same contract
through `eval_module_source`.

## `crates/thinpack-jsc-rewrite`

The system-framework build has no module C API, so its rewriter lowers imports and exports
into a function body. That rewriter used to be a set of hand-rolled scanners in
`src/jsc/module.rs`, each deciding for itself what was code. It now lives in the path
dependency `crates/thinpack-jsc-rewrite`; `src/jsc/module.rs` keeps only the JSC shim and
calls it. The crate has no external dependencies, so the linked code is the same size class
as the scanners it replaces, and it is unit-tested on every platform
(`cargo test --manifest-path crates/thinpack-jsc-rewrite/Cargo.toml`). The crate is thinpack's (MIT, `LICENSE` beside it);
it lives in this fork because the shim is its only consumer.

One lexer in that crate recognizes comments, strings, template literals with nested `${}`
expressions, regular expressions, identifiers, numbers and punctuators. Every pass uses it,
which fixes the defects that came from scanning raw text:

- `import.meta` is replaced only when it is the three tokens `import . meta`, not inside a
  string, template text, comment or regular expression (previously a whole-body
  `str::replace`).
- Regular-expression-vs-division uses the surrounding expression context. A `/` after the
  `)` of `if`/`while`/`for`/`with` starts a regex; after an ordinary parenthesized expression
  it is division, and `/=` opens a regex in expression position.
- Brace matching respects nesting, so `export const { a: { b } } = o` exports every binding,
  including array patterns, defaults, computed keys and rest elements.
- Statement splitting at `;` looks at tokens, not characters, so a semicolon in a string no
  longer creates a statement.
- Dynamic-import rewriting only matches an `import` token followed by `(`, so `"import(x)"`,
  comments, templates and `obj.import(...)` are left alone.
- Import locations are recorded from specifier-token offsets instead of a first-substring
  scan.

This is still a text rewrite, not a parser. Statement recognition stays line-oriented, the
asynchronous wrapper still relies on the synchronous parse check for `await` inside a
top-level block, and import-attribute extraction remains a source scan.

## Validation

The startup patch is covered by the differential runs below, comparing the stock
quickjs-ng 0.15.1 source against this tree; the undefined-specifier fix is covered by
`packed::esm-invalid-specifier-raw-nul`, own-only enumeration by the Node `buffer`, `util`
and `assert` suites, top-level await by the Node dynamic-import cases, link checks and
hoisted declarations by `packed::esm-link-error`, `packed::esm-large-link-error` and
`packed::esm-dynamic-link-error`, and the rewriter by its unit tests:

- quickjs-ng 0.15.1 and 0.18 test scripts: identical output and exit status.
- 703 focused test262 files (`let`, `const`, `block-scope`, `identifier-resolution`,
  `arguments-object`): identical results, including all 145 `let` tests.
- 600 generated programs with nested blocks, shadowing, `with`, catch bindings, default
  parameters, eval, and synchronous function redeclaration: identical output and exit status.
- The thinpack Node.js compatibility suite: 227 passed, 0 failed.
- The packed suite (including both startup guards): 21 passed, 0 failed.
- The rewrite crate's Linux unit tests: 18 passed.

## Updating

1. Fetch the target `denoland/v8x` release commit and rebase this branch onto it.
2. Regenerate `patches/quickjs-103-thinpack-startup.patch` against the new submodule pin
   after upstream's numbered patches, and keep it the last quickjs patch.
3. Keep the upstream column fix if the new quickjs-ng already contains it; drop that hunk
   from the patch instead of reverting upstream.
4. Rebase the JSC changes onto the new `src/jsc` files and rerun `cargo xtask lint` in
   thinpack. On macOS that also compiles the system-JSC backend.
