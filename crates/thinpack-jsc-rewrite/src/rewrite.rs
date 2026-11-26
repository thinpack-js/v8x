//! Token-aware rewriting of ES module syntax for the system-JavaScriptCore backend.
//!
//! `JavaScriptCore`'s public C API has no module loader, so the backend turns a module
//! into a function body whose imports read from a namespace registry and whose
//! exports assign into its own namespace object. The rewrite is only used for the
//! macOS system framework; the vendored JSC build links the real `JSModuleRecord`.
//!
//! Everything here is driven by [`crate::lexer`] tokens rather than raw text, so
//! strings, template literals, comments, and regular expressions can never be
//! mistaken for module syntax.

use std::collections::HashMap;

use crate::lexer::{Event, Kind, Lexer, Token, token_events, tokens};

/// A static `import` or `export ... from` request found by the lexical scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaticImport {
  /// The specifier literal, without its quotes.
  pub specifier: String,
  /// Byte offset of the specifier literal's opening quote.
  pub offset: usize,
  /// `type` from the request's `with { ... }` or `assert { ... }` clause.
  pub attribute_type: Option<String>,
}

/// A rewritten module body and the metadata the JSC wrapper needs.
#[derive(Debug)]
pub struct RewrittenModule {
  /// The function body with imports and exports lowered to namespace access.
  pub body: String,
  /// Every name the rewritten module puts on its namespace.
  pub export_names: Vec<String>,
  /// Requested module specifiers, in source order.
  pub imports: Vec<String>,
  /// `(specifier, imported name, 1-based source line)` for the link check.
  pub import_checks: Vec<(String, String, usize)>,
  /// Whether a dependency is re-exported with `export *`, which hides its names.
  pub has_star_reexports: bool,
  /// Whether the module contains top-level `await`.
  pub is_async: bool,
  /// `(exported, specifier, source name)` for live `export ... from` bindings.
  pub reexports: Vec<(String, String, String)>,
}

/// Lower `src` from ES module syntax to a function body, or `None` when the
/// module uses a form the rewriter cannot represent.
#[must_use]
pub fn rewrite_es_module(src: &str) -> Option<RewrittenModule> {
  let cleaned = strip_comments(src);
  let mut rewriter = Rewriter::new(src);
  for line in logical_statements(&cleaned) {
    rewriter.statement(&line)?;
  }
  Some(rewriter.finish())
}

/// Whether `src` contains `await` in module scope (best effort; a module wrapped
/// synchronously is retried as async by the caller, so nested blocks may be missed).
#[must_use]
pub fn has_top_level_await(src: &str) -> bool {
  let mut depth = 0i32;
  let mut brackets = 0usize;
  // Expression-bodied arrows: (brace depth, bracket nesting) where the body began.
  let mut arrow_expressions: Vec<(i32, usize)> = Vec::new();
  let mut prev_dot = false;

  for token in token_events(src) {
    if token.kind == Kind::Punct {
      match token.text {
        "(" | "[" => brackets += 1,
        ")" | "]" => {
          brackets = brackets.saturating_sub(1);
          while arrow_expressions
            .last()
            .is_some_and(|(_, nesting)| *nesting > brackets)
          {
            arrow_expressions.pop();
          }
        }
        "{" => depth += 1,
        "}" => {
          depth -= 1;
          while arrow_expressions
            .last()
            .is_some_and(|(brace_depth, _)| *brace_depth > depth)
          {
            arrow_expressions.pop();
          }
        }
        "=>" => arrow_expressions.push((depth, brackets)),
        "," | ";" => {
          while arrow_expressions.last().is_some_and(
            |(brace_depth, nesting)| {
              *brace_depth == depth && *nesting == brackets
            },
          ) {
            arrow_expressions.pop();
          }
        }
        _ => {}
      }
    } else if token.is_ident("await")
      && !prev_dot
      && depth == 0
      && arrow_expressions.is_empty()
    {
      return true;
    }
    prev_dot = token.is_punct(".");
  }
  false
}

/// Rewrite `import(` to `__v82jsc_dynImport(`, leaving strings and comments alone.
#[must_use]
pub fn rewrite_dynamic_import_calls(body: &str) -> String {
  let mut ts = token_events(body).peekable();
  let mut out = String::with_capacity(body.len());
  let mut cursor = 0;
  let mut prev: Option<Token<'_>> = None;
  while let Some(token) = ts.next() {
    let is_call = token.is_ident("import")
      && ts.peek().is_some_and(|next| next.is_punct("("))
      && prev.is_none_or(|prev| !prev.is_punct("."));
    if is_call {
      out.push_str(&body[cursor..token.start]);
      out.push_str("__v82jsc_dynImport");
      cursor = token.end;
    }
    prev = Some(token);
  }
  out.push_str(&body[cursor..]);
  out
}

/// Byte offset of each specifier literal in `text`, in request order.
///
/// Repeated specifiers map to their successive occurrences. A specifier that is
/// not found maps to `0`, the offset V8 reports for a missing location.
#[must_use]
pub fn import_offsets(text: &str, specifiers: &[String]) -> Vec<usize> {
  let entries = static_imports(text);
  let mut cursor = 0;
  specifiers
    .iter()
    .map(|specifier| {
      let hit = entries
        .iter()
        .skip(cursor)
        .position(|entry| entry.specifier == *specifier);
      hit.map_or(0, |hit| {
        let index = cursor + hit;
        cursor = index + 1;
        entries[index].offset
      })
    })
    .collect()
}

struct Rewriter<'a> {
  src: &'a str,
  body: String,
  imports_src: String,
  exports_src: String,
  export_names: Vec<String>,
  imports: Vec<String>,
  import_checks: Vec<(String, String, usize)>,
  /// Local binding -> `(specifier, source export name)` for imported bindings.
  import_bindings: HashMap<String, (String, String)>,
  star_reexports: Vec<(String, Option<String>)>,
  reexports: Vec<(String, String, String)>,
  specifier_lines: SpecifierLines,
}

impl<'a> Rewriter<'a> {
  fn new(src: &'a str) -> Self {
    Self {
      src,
      body: String::new(),
      imports_src: String::new(),
      exports_src: String::new(),
      export_names: Vec::new(),
      imports: Vec::new(),
      import_checks: Vec::new(),
      import_bindings: HashMap::new(),
      star_reexports: Vec::new(),
      reexports: Vec::new(),
      specifier_lines: SpecifierLines::new(src),
    }
  }

  fn statement(&mut self, raw_line: &str) -> Option<()> {
    let line = raw_line.trim();
    let ts = tokens(line);
    let Some(first) = ts.first().copied() else {
      self.push_body(raw_line);
      return Some(());
    };
    if first.is_ident("import") {
      if matches!(ts.get(1), Some(next) if next.is_punct("(") || next.is_punct("."))
      {
        self.push_body(raw_line);
        return Some(());
      }
      self.import_statement(line, &ts);
      return Some(());
    }
    if first.is_ident("export") {
      return match ts.get(1).copied() {
        Some(second) if second.is_punct("*") => self.star_export(line),
        Some(second) if second.is_punct("{") => {
          self.named_export(line, &ts);
          Some(())
        }
        Some(second) if second.is_ident("default") => {
          self.default_export(line, second.end);
          Some(())
        }
        _ => {
          self.declaration_export(line);
          Some(())
        }
      };
    }
    self.push_body(raw_line);
    Some(())
  }

  fn push_body(&mut self, line: &str) {
    self.body.push_str(line);
    self.body.push('\n');
  }

  fn import_statement(&mut self, stmt: &str, ts: &[Token<'_>]) {
    let specifier = specifier_of(stmt).unwrap_or_default();
    let line = if specifier.is_empty() {
      1
    } else {
      self.specifier_lines.line_for(&specifier)
    };
    if !specifier.is_empty() {
      self.imports.push(specifier.clone());
    }
    let module = module_expr(&specifier);
    let clause = import_clause(ts);
    if let Some(name) = clause.default {
      self
        .imports_src
        .push_str(&format!("const {name} = {module}.default;\n"));
      self
        .import_bindings
        .insert(name, (specifier.clone(), "default".to_owned()));
      self
        .import_checks
        .push((specifier.clone(), "default".to_owned(), line));
    }
    if let Some(name) = clause.namespace {
      self
        .imports_src
        .push_str(&format!("const {name} = {module};\n"));
      self
        .import_bindings
        .insert(name, (specifier.clone(), "*".to_owned()));
    }
    if !clause.named.is_empty() {
      let mut destructure = Vec::with_capacity(clause.named.len());
      for (imported, local) in clause.named {
        if imported == local {
          destructure.push(imported.clone());
        } else {
          destructure.push(format!("{imported}: {local}"));
        }
        self
          .import_bindings
          .insert(local, (specifier.clone(), imported.clone()));
        self.import_checks.push((specifier.clone(), imported, line));
      }
      self.imports_src.push_str(&format!(
        "const {{ {} }} = {module};\n",
        destructure.join(", ")
      ));
    }
  }

  fn star_export(&mut self, stmt: &str) -> Option<()> {
    let specifier = specifier_of(stmt).unwrap_or_default();
    if specifier.is_empty() {
      return None;
    }
    let _ = self.specifier_lines.line_for(&specifier);
    self.imports.push(specifier.clone());
    let as_name = star_as_name(stmt);
    if let Some(name) = &as_name {
      self.export_names.push(name.clone());
    }
    self.star_reexports.push((specifier, as_name));
    Some(())
  }

  fn named_export(&mut self, stmt: &str, ts: &[Token<'_>]) {
    let specifier = if ts.iter().any(|token| token.is_ident("from")) {
      specifier_of(stmt).filter(|specifier| !specifier.is_empty())
    } else {
      None
    };
    let mut line = 1;
    if let Some(specifier) = &specifier {
      line = self.specifier_lines.line_for(specifier);
      self.imports.push(specifier.clone());
    }
    for (local, exported) in named_pairs(ts) {
      self.export_names.push(exported.clone());
      if let Some(specifier) = &specifier {
        self
          .reexports
          .push((exported, specifier.clone(), local.clone()));
        self.import_checks.push((specifier.clone(), local, line));
      } else if let Some((source, source_name)) =
        self.import_bindings.get(&local).cloned()
      {
        self.reexports.push((exported, source, source_name));
      } else {
        self
          .exports_src
          .push_str(&format!("__ns[{exported:?}] = {local};\n"));
      }
    }
  }

  fn default_export(&mut self, stmt: &str, default_end: usize) {
    self.export_names.push("default".to_owned());
    self.body.push_str("const __v8jsc_default = ");
    self.body.push_str(stmt[default_end..].trim_start());
    self.body.push('\n');
    self
      .exports_src
      .push_str("__ns[\"default\"] = __v8jsc_default;\n");
  }

  fn declaration_export(&mut self, stmt: &str) {
    let Some(first) = tokens(stmt).first().copied() else {
      self.push_body(stmt);
      return;
    };
    let declaration = stmt[first.end..].trim_start();
    let Some(target) = declaration_target(declaration) else {
      self.push_body(stmt);
      return;
    };
    self.push_body(declaration);
    if target.starts_with(['{', '[']) {
      for name in binding_names(target) {
        self.export_names.push(name.clone());
        self
          .exports_src
          .push_str(&format!("__ns[{name:?}] = {name};\n"));
      }
    } else if let Some(name) = leading_ident(target) {
      self.export_names.push(name.clone());
      self
        .exports_src
        .push_str(&format!("__ns[{name:?}] = {name};\n"));
    }
  }

  fn finish(mut self) -> RewrittenModule {
    for (specifier, as_name) in &self.star_reexports {
      let module = module_expr(specifier);
      match as_name {
                Some(name) if !name.is_empty() => self
                    .exports_src
                    .push_str(&format!("__ns[{name:?}] = {module};\n")),
                _ => self.exports_src.push_str(&format!(
                    "{{const __se={module};for(const __k of Object.keys(__se)){{\
             if(__k!==\"default\"&&!Object.prototype.hasOwnProperty.call(__ns,__k)){{\
               Object.defineProperty(__ns,__k,{{get:((k)=>()=>__se[k])(__k),\
                 enumerable:true,configurable:true}});}}}}}}\n"
                )),
            }
    }
    let combined =
      format!("{}{}{}", self.imports_src, self.body, self.exports_src);
    let body = rewrite_import_meta(&combined);
    RewrittenModule {
      body,
      export_names: self.export_names,
      imports: self.imports,
      import_checks: self.import_checks,
      has_star_reexports: !self.star_reexports.is_empty(),
      is_async: has_top_level_await(self.src),
      reexports: self.reexports,
    }
  }
}

/// Replace `import.meta` with the backend's placeholder, token by token.
fn rewrite_import_meta(src: &str) -> String {
  let ts = tokens(src);
  let mut out = String::with_capacity(src.len());
  let mut cursor = 0;
  for index in 2..ts.len() {
    let is_meta = ts[index].is_ident("meta")
      && ts[index - 1].is_punct(".")
      && ts[index - 2].is_ident("import")
      && (index < 3 || !ts[index - 3].is_punct("."));
    if is_meta {
      out.push_str(&src[cursor..ts[index - 2].start]);
      out.push_str("__v8jsc_meta");
      cursor = ts[index].end;
    }
  }
  out.push_str(&src[cursor..]);
  out
}

/// Replace comments with spaces, keeping line breaks so the line-based statement
/// splitter sees the same lines as the original source.
pub(crate) fn strip_comments(src: &str) -> String {
  let mut out = String::with_capacity(src.len());
  let mut cursor = 0;
  for event in Lexer::new(src) {
    if let Event::Comment(comment) = event {
      out.push_str(&src[cursor..comment.start]);
      out.extend(
        src[comment.clone()]
          .chars()
          .map(|character| if character == '\n' { '\n' } else { ' ' }),
      );
      cursor = comment.end;
    }
  }
  out.push_str(&src[cursor..]);
  out
}

fn logical_statements(cleaned: &str) -> Vec<String> {
  join_module_statements(cleaned)
    .iter()
    .flat_map(|line| split_module_statements(line))
    .collect()
}

/// Joins a named-binding list broken over lines (`import {` newline `a,` ...).
fn join_module_statements(src: &str) -> Vec<String> {
  let lines: Vec<&str> = src.lines().collect();
  let mut result = Vec::new();
  let mut index = 0;
  while index < lines.len() {
    let trimmed = lines[index].trim();
    // Only JOIN true named-binding lists (`import {`, `export {`, `import a, {`).
    // A declaration like `export class X {` also starts with `export` and has an
    // unclosed `{`, but its `{` opens a body; joining would swallow the header.
    let after_keyword = trimmed
      .strip_prefix("import")
      .or_else(|| trimmed.strip_prefix("export"))
      .map_or("", str::trim_start);
    let import_binding = trimmed.starts_with("import")
      && (after_keyword.starts_with('{')
        || after_keyword
          .split_once(',')
          .is_some_and(|(_, rest)| rest.trim_start().starts_with('{')));
    let export_binding =
      trimmed.starts_with("export") && after_keyword.starts_with('{');
    let starts_brace_statement = (import_binding || export_binding)
      && trimmed.contains('{')
      && !trimmed.contains('}');
    if !starts_brace_statement {
      result.push(lines[index].to_owned());
      index += 1;
      continue;
    }
    let mut joined = String::new();
    joined.push_str(lines[index].trim_end());
    joined.push(' ');
    index += 1;
    while index < lines.len() {
      let line = lines[index];
      joined.push_str(line.trim());
      joined.push(' ');
      index += 1;
      if !line.contains('}') {
        continue;
      }
      if !joined.contains(" from ")
        && !joined.trim_end().ends_with(';')
        && index < lines.len()
        && lines[index].trim_start().starts_with("from ")
      {
        joined.push_str(lines[index].trim());
        index += 1;
      }
      break;
    }
    result.push(joined.trim().to_owned());
  }
  result
}

/// Splits a line where `;` is followed by an import/export declaration, so a
/// hoisted declaration after a statement is still recognized.
fn split_module_statements(line: &str) -> Vec<String> {
  let ts = tokens(line);
  let mut parts = Vec::new();
  let mut start = 0;
  for (index, token) in ts.iter().enumerate() {
    if !token.is_punct(";") {
      continue;
    }
    let next = ts.get(index + 1);
    if matches!(next, Some(next) if next.is_ident("import") || next.is_ident("export"))
    {
      parts.push(line[start..token.end].to_owned());
      start = token.end;
    }
  }
  parts.push(line[start..].to_owned());
  parts.retain(|part| !part.trim().is_empty());
  parts
}

fn module_expr(specifier: &str) -> String {
  format!("((globalThis.__v8jsc_modules||{{}})[{specifier:?}]||{{}})")
}

/// The specifier of an import/export declaration.
fn specifier_of(stmt: &str) -> Option<String> {
  let ts = tokens(stmt);
  let from = ts.iter().enumerate().position(|(index, token)| {
    index > 0
      && token.is_ident("from")
      && ts
        .get(index + 1)
        .is_some_and(|next| next.kind == Kind::String)
  });
  if let Some(from) = from {
    return ts
      .get(from + 1)
      .and_then(|token| token.string_value())
      .map(str::to_owned);
  }
  if ts.first().is_some_and(|first| first.is_ident("import")) {
    return ts
      .iter()
      .skip(1)
      .find(|token| token.kind == Kind::String)
      .and_then(|token| token.string_value())
      .map(str::to_owned);
  }
  None
}

fn star_as_name(stmt: &str) -> Option<String> {
  let ts = tokens(stmt);
  let star = ts.iter().position(|token| token.is_punct("*"))?;
  if !ts.get(star + 1)?.is_ident("as") {
    return None;
  }
  let name = ts.get(star + 2)?;
  (name.kind == Kind::Ident).then(|| name.text.to_owned())
}

#[derive(Debug, Default)]
struct ImportClause {
  default: Option<String>,
  namespace: Option<String>,
  named: Vec<(String, String)>,
}

fn import_clause(ts: &[Token<'_>]) -> ImportClause {
  let from = ts.iter().enumerate().position(|(index, token)| {
    index > 0
      && token.is_ident("from")
      && ts
        .get(index + 1)
        .is_some_and(|next| next.kind == Kind::String)
  });
  let Some(from) = from else {
    return ImportClause::default();
  };
  let mut clause = ImportClause::default();
  for part in split_commas(&ts[1..from]) {
    let Some(first) = part.first() else {
      continue;
    };
    if first.is_punct("{") {
      clause.named = named_pairs(part);
    } else if first.is_punct("*") {
      if part.get(1).is_some_and(|token| token.is_ident("as")) {
        clause.namespace = part
          .get(2)
          .filter(|token| token.kind == Kind::Ident)
          .map(|token| token.text.to_owned());
      }
    } else if first.kind == Kind::Ident {
      clause.default = Some(first.text.to_owned());
    }
  }
  clause
}

/// `(local, exported)` pairs of a `{ ... }` import or export list.
fn named_pairs(ts: &[Token<'_>]) -> Vec<(String, String)> {
  let Some(open) = ts.iter().position(|token| token.is_punct("{")) else {
    return Vec::new();
  };
  let mut depth = 0usize;
  let mut close = ts.len();
  for (index, token) in ts.iter().enumerate().skip(open) {
    if token.is_punct("{") {
      depth += 1;
    } else if token.is_punct("}") {
      depth = depth.saturating_sub(1);
      if depth == 0 {
        close = index;
        break;
      }
    }
  }
  let mut pairs = Vec::new();
  for part in split_commas(&ts[open + 1..close]) {
    let Some(first) = part.first() else {
      continue;
    };
    let Some(local) = specifier_name(*first) else {
      continue;
    };
    let exported = if part.get(1).is_some_and(|token| token.is_ident("as")) {
      part
        .get(2)
        .and_then(|token| specifier_name(*token))
        .unwrap_or_else(|| local.clone())
    } else {
      local.clone()
    };
    pairs.push((local, exported));
  }
  pairs
}

fn specifier_name(token: Token<'_>) -> Option<String> {
  match token.kind {
    Kind::Ident => Some(token.text.to_owned()),
    Kind::String => token.string_value().map(str::to_owned),
    _ => None,
  }
}

fn split_commas<'a, 'b>(ts: &'a [Token<'b>]) -> Vec<&'a [Token<'b>]> {
  let mut parts = Vec::new();
  let mut depth = 0usize;
  let mut start = 0;
  for (index, token) in ts.iter().enumerate() {
    if token.is_punct("(") || token.is_punct("[") || token.is_punct("{") {
      depth += 1;
    } else if token.is_punct(")") || token.is_punct("]") || token.is_punct("}")
    {
      depth = depth.saturating_sub(1);
    } else if depth == 0 && token.is_punct(",") {
      parts.push(&ts[start..index]);
      start = index + 1;
    }
  }
  parts.push(&ts[start..]);
  parts
}

/// The declaration text after `export`, with its keyword removed.
fn declaration_target(declaration: &str) -> Option<&str> {
  let declaration = keyword_rest(declaration, "async").unwrap_or(declaration);
  for keyword in ["const", "let", "var", "function", "class"] {
    if let Some(target) = keyword_rest(declaration, keyword) {
      return Some(if keyword == "function" {
        target.trim_start_matches('*').trim_start()
      } else {
        target
      });
    }
  }
  None
}

fn keyword_rest<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
  let rest = text.strip_prefix(keyword)?;
  match rest.chars().next() {
    None => Some(rest),
    Some(character)
      if character.is_whitespace() || matches!(character, '{' | '[' | '*') =>
    {
      Some(rest.trim_start())
    }
    Some(_) => None,
  }
}

fn leading_ident(text: &str) -> Option<String> {
  let name: String = text
    .chars()
    .take_while(|character| {
      character.is_alphanumeric() || matches!(character, '_' | '$')
    })
    .collect();
  (!name.is_empty()).then_some(name)
}

pub(crate) fn binding_names(pattern: &str) -> Vec<String> {
  let ts = tokens(pattern);
  let mut names = Vec::new();
  let mut pos = 0;
  collect_pattern(&ts, &mut pos, &mut names);
  names
}

fn collect_pattern(ts: &[Token<'_>], pos: &mut usize, names: &mut Vec<String>) {
  match ts.get(*pos) {
    Some(token) if token.is_punct("{") => {
      collect_object_pattern(ts, pos, names)
    }
    Some(token) if token.is_punct("[") => collect_array_pattern(ts, pos, names),
    Some(token) if token.kind == Kind::Ident => {
      names.push(token.text.to_owned());
      *pos += 1;
    }
    // Malformed patterns are the engine's to reject; just keep moving.
    Some(_) => *pos += 1,
    None => {}
  }
}

fn collect_object_pattern(
  ts: &[Token<'_>],
  pos: &mut usize,
  names: &mut Vec<String>,
) {
  *pos += 1;
  while let Some(token) = ts.get(*pos) {
    if token.is_punct("}") {
      *pos += 1;
      return;
    }
    if token.is_punct(",") {
      *pos += 1;
      continue;
    }
    if is_spread(ts, *pos) {
      *pos += 3;
      collect_pattern(ts, pos, names);
      continue;
    }
    if token.is_punct("[") {
      skip_bracket(ts, pos);
      if ts.get(*pos).is_some_and(|token| token.is_punct(":")) {
        *pos += 1;
      }
      collect_pattern(ts, pos, names);
      skip_default(ts, pos);
      continue;
    }
    let key = *token;
    *pos += 1;
    if ts.get(*pos).is_some_and(|token| token.is_punct(":")) {
      *pos += 1;
      collect_pattern(ts, pos, names);
    } else if key.kind == Kind::Ident {
      names.push(key.text.to_owned());
    }
    skip_default(ts, pos);
  }
}

fn collect_array_pattern(
  ts: &[Token<'_>],
  pos: &mut usize,
  names: &mut Vec<String>,
) {
  *pos += 1;
  while let Some(token) = ts.get(*pos) {
    if token.is_punct("]") {
      *pos += 1;
      return;
    }
    if token.is_punct(",") {
      *pos += 1;
      continue;
    }
    if is_spread(ts, *pos) {
      *pos += 3;
    }
    collect_pattern(ts, pos, names);
    skip_default(ts, pos);
  }
}

fn is_spread(ts: &[Token<'_>], pos: usize) -> bool {
  [0, 1, 2].iter().all(|offset| {
    ts.get(pos + offset)
      .is_some_and(|token| token.is_punct("."))
  })
}

fn skip_bracket(ts: &[Token<'_>], pos: &mut usize) {
  let mut depth = 0usize;
  while let Some(token) = ts.get(*pos) {
    if token.is_punct("[") {
      depth += 1;
    } else if token.is_punct("]") {
      depth = depth.saturating_sub(1);
      *pos += 1;
      if depth == 0 {
        return;
      }
      continue;
    }
    *pos += 1;
  }
}

/// Skips a `= expression` default, stopping before the enclosing separator.
fn skip_default(ts: &[Token<'_>], pos: &mut usize) {
  if !ts.get(*pos).is_some_and(|token| token.is_punct("=")) {
    return;
  }
  *pos += 1;
  let mut depth = 0usize;
  while let Some(token) = ts.get(*pos) {
    if token.is_punct("(") || token.is_punct("[") || token.is_punct("{") {
      depth += 1;
    } else if token.is_punct(")") || token.is_punct("]") || token.is_punct("}")
    {
      if depth == 0 {
        return;
      }
      depth -= 1;
    } else if depth == 0 && token.is_punct(",") {
      return;
    }
    *pos += 1;
  }
}

/// Every static `import` and `export ... from` request in source order.
///
/// The token stream makes comments, strings, template literals, and regular
/// expressions opaque, so module syntax is found only where it is real.
#[must_use]
pub fn static_imports(src: &str) -> Vec<StaticImport> {
  let ts = tokens(src);
  let mut imports = Vec::new();
  for (index, token) in ts.iter().enumerate() {
    let prev_dot = index > 0 && ts[index - 1].is_punct(".");
    if token.is_ident("import") && !prev_dot {
      match ts.get(index + 1) {
        Some(next) if next.kind == Kind::String => {
          if let Some(value) = next.string_value() {
            imports.push(StaticImport {
              specifier: value.to_owned(),
              offset: next.start,
              attribute_type: import_attribute_type(&ts, index + 1),
            });
          }
        }
        Some(next) if next.is_punct("(") || next.is_punct(".") => {}
        _ => {
          if let Some((spec_index, offset, specifier)) =
            from_specifier(&ts, src, index)
          {
            imports.push(StaticImport {
              specifier,
              offset,
              attribute_type: import_attribute_type(&ts, spec_index),
            });
          }
        }
      }
    } else if token.is_ident("export")
      && matches!(ts.get(index + 1), Some(next) if next.is_punct("*") || next.is_punct("{"))
      && let Some((spec_index, offset, specifier)) =
        from_specifier(&ts, src, index)
    {
      imports.push(StaticImport {
        specifier,
        offset,
        attribute_type: import_attribute_type(&ts, spec_index),
      });
    }
  }
  imports
}

/// The `from` specifier of a declaration as `(token index, byte offset, value)`.
fn from_specifier(
  ts: &[Token<'_>],
  src: &str,
  start: usize,
) -> Option<(usize, usize, String)> {
  let mut depth = 0usize;
  let mut previous_end = ts.get(start)?.end;
  for (index, token) in ts.iter().enumerate().skip(start + 1) {
    if token.is_punct("(") || token.is_punct("[") || token.is_punct("{") {
      depth += 1;
    } else if token.is_punct(")") || token.is_punct("]") || token.is_punct("}")
    {
      depth = depth.saturating_sub(1);
    } else if depth == 0 {
      if token.is_ident("from") {
        if let Some(next) = ts.get(index + 1)
          && next.kind == Kind::String
        {
          return next
            .string_value()
            .map(|value| (index + 1, next.start, value.to_owned()));
        }
      } else if token.is_punct(";")
        || starts_statement(src, previous_end, *token)
      {
        return None;
      }
    }
    previous_end = token.end;
  }
  None
}

/// The `type` value from the clause immediately after a specifier literal.
fn import_attribute_type(
  ts: &[Token<'_>],
  specifier_index: usize,
) -> Option<String> {
  let keyword = ts.get(specifier_index + 1)?;
  if !keyword.is_ident("with") && !keyword.is_ident("assert") {
    return None;
  }
  if !ts.get(specifier_index + 2)?.is_punct("{") {
    return None;
  }
  let mut index = specifier_index + 3;
  while let Some(key) = ts.get(index) {
    if key.is_punct("}") {
      return None;
    }
    let name = match key.kind {
      Kind::Ident => key.text,
      Kind::String => key.string_value()?,
      _ => return None,
    };
    if !ts.get(index + 1)?.is_punct(":") {
      return None;
    }
    let value = ts.get(index + 2)?;
    if value.kind != Kind::String {
      return None;
    }
    if name == "type" {
      return value.string_value().map(str::to_owned);
    }
    index += 3;
    if ts.get(index).is_some_and(|token| token.is_punct(",")) {
      index += 1;
    }
  }
  None
}

fn starts_statement(src: &str, previous_end: usize, token: Token<'_>) -> bool {
  if token.kind != Kind::Ident || !src[previous_end..token.start].contains('\n')
  {
    return false;
  }
  matches!(
    token.text,
    "class"
      | "const"
      | "do"
      | "export"
      | "for"
      | "function"
      | "if"
      | "import"
      | "let"
      | "return"
      | "switch"
      | "throw"
      | "try"
      | "var"
      | "while"
  )
}

struct SpecifierLines {
  entries: Vec<StaticImport>,
  line_starts: Vec<usize>,
  cursor: usize,
}

impl SpecifierLines {
  fn new(src: &str) -> Self {
    Self {
      entries: static_imports(src),
      line_starts: line_starts(src),
      cursor: 0,
    }
  }

  /// 1-based line of the next specifier matching `specifier`.
  fn line_for(&mut self, specifier: &str) -> usize {
    let Some(hit) = self
      .entries
      .iter()
      .skip(self.cursor)
      .position(|entry| entry.specifier == specifier)
    else {
      return 1;
    };
    let index = self.cursor + hit;
    self.cursor = index + 1;
    let offset = self.entries[index].offset;
    self.line_starts.partition_point(|start| *start <= offset)
  }
}

fn line_starts(src: &str) -> Vec<usize> {
  let mut starts = vec![0];
  for (index, byte) in src.bytes().enumerate() {
    if byte == b'\n' {
      starts.push(index + 1);
    }
  }
  starts
}
