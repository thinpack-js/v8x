//! The lexical scanner behind the module rewrite.
//!
//! It recognizes exactly what the rewriter looks at: comments, identifiers, string
//! and template literals, regular-expression literals, numbers, and punctuators.
//! The only non-local state is what distinguishes a regular expression from a
//! division operator, which needs the surrounding expression context.

use std::ops::Range;

/// The token classes the rewriter distinguishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
  Ident,
  Punct,
  String,
  Number,
  Regex,
}

/// A significant token and its byte span in the source.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Token<'a> {
  pub(crate) kind: Kind,
  pub(crate) text: &'a str,
  pub(crate) start: usize,
  pub(crate) end: usize,
}

impl<'a> Token<'a> {
  pub(crate) fn is_ident(self, name: &str) -> bool {
    self.kind == Kind::Ident && self.text == name
  }

  pub(crate) fn is_punct(self, punct: &str) -> bool {
    self.kind == Kind::Punct && self.text == punct
  }

  /// The contents of a string literal, without its quotes.
  pub(crate) fn string_value(self) -> Option<&'a str> {
    if self.kind != Kind::String {
      return None;
    }
    self.text.get(1..self.text.len().saturating_sub(1))
  }
}

/// What the scanner found between two significant tokens.
#[derive(Debug)]
pub(crate) enum Event<'a> {
  Token(Token<'a>),
  Comment(Range<usize>),
}

/// A tokenizer over one source text.
#[expect(
  clippy::struct_excessive_bools,
  reason = "four independent one-token facts make up the expression context"
)]
pub(crate) struct Lexer<'a> {
  src: &'a str,
  bytes: &'a [u8],
  pos: usize,
  /// Whether a regular-expression literal may start at the current position.
  expr_allowed: bool,
  /// Whether the next `{` opens a statement block (`if (x) {}`, `else {}`).
  block_expected: bool,
  prev_dot: bool,
  prev_control: bool,
  /// For every open `(`, whether it belongs to `if`/`while`/`for`/`with`.
  parens: Vec<bool>,
  braces: Vec<Brace>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Brace {
  Block,
  Object,
  Template,
}

impl<'a> Lexer<'a> {
  pub(crate) const fn new(src: &'a str) -> Self {
    Self {
      src,
      bytes: src.as_bytes(),
      pos: 0,
      expr_allowed: true,
      block_expected: false,
      prev_dot: false,
      prev_control: false,
      parens: Vec::new(),
      braces: Vec::new(),
    }
  }

  fn byte_at(&self, offset: usize) -> Option<u8> {
    self.bytes.get(self.pos + offset).copied()
  }

  fn token(&self, kind: Kind, start: usize) -> Token<'a> {
    Token {
      kind,
      text: &self.src[start..self.pos],
      start,
      end: self.pos,
    }
  }

  /// Skips whitespace and returns one comment if it stops at one.
  fn skip_trivia(&mut self) -> Option<Range<usize>> {
    while self
      .byte_at(0)
      .is_some_and(|byte| byte.is_ascii_whitespace())
    {
      self.pos += 1;
    }
    let start = self.pos;
    match (self.byte_at(0), self.byte_at(1)) {
      (Some(b'/'), Some(b'/')) => {
        self.pos += 2;
        while self.byte_at(0).is_some_and(|byte| byte != b'\n') {
          self.pos += 1;
        }
        Some(start..self.pos)
      }
      (Some(b'/'), Some(b'*')) => {
        self.pos += 2;
        while self.pos + 1 < self.bytes.len() {
          if self.byte_at(0) == Some(b'*') && self.byte_at(1) == Some(b'/') {
            self.pos += 2;
            return Some(start..self.pos);
          }
          self.pos += 1;
        }
        self.pos = self.bytes.len();
        Some(start..self.pos)
      }
      _ => None,
    }
  }

  fn next_token(&mut self) -> Option<Token<'a>> {
    let start = self.pos;
    let byte = self.bytes[start];
    let after_dot = self.prev_dot;
    let after_control = self.prev_control;
    let expect_block = self.block_expected;
    self.prev_dot = false;
    self.prev_control = false;
    self.block_expected = false;

    if is_ident_start(byte) {
      return Some(self.scan_ident(start, after_dot));
    }

    if byte.is_ascii_digit()
      || (byte == b'.' && self.byte_at(1).is_some_and(|b| b.is_ascii_digit()))
    {
      self.scan_number();
      self.expr_allowed = false;
      return Some(self.token(Kind::Number, start));
    }

    match byte {
      b'`' => {
        self.pos += 1;
        self.expr_allowed = false;
        if self.template_text() {
          self.braces.push(Brace::Template);
          self.expr_allowed = true;
        }
        None
      }
      b'"' | b'\'' => {
        self.scan_string(byte);
        self.expr_allowed = false;
        Some(self.token(Kind::String, start))
      }
      b'/' => Some(self.slash(start)),
      b'(' => {
        self.parens.push(after_control);
        self.pos += 1;
        self.expr_allowed = true;
        Some(self.token(Kind::Punct, start))
      }
      b')' => {
        self.pos += 1;
        self.expr_allowed = self.parens.pop().unwrap_or(false);
        self.block_expected = self.expr_allowed;
        Some(self.token(Kind::Punct, start))
      }
      b'{' => {
        let brace = if expect_block || !self.expr_allowed {
          Brace::Block
        } else {
          Brace::Object
        };
        self.braces.push(brace);
        self.pos += 1;
        self.expr_allowed = true;
        Some(self.token(Kind::Punct, start))
      }
      b'}' => self.close_brace(),
      b']' => {
        self.pos += 1;
        self.expr_allowed = false;
        Some(self.token(Kind::Punct, start))
      }
      b'.' => {
        self.pos += 1;
        self.prev_dot = true;
        self.expr_allowed = false;
        Some(self.token(Kind::Punct, start))
      }
      b'=' if self.byte_at(1) == Some(b'>') => {
        self.pos += 2;
        self.expr_allowed = true;
        self.block_expected = true;
        Some(self.token(Kind::Punct, start))
      }
      b'+' | b'-' if self.byte_at(1) == Some(byte) => {
        self.pos += 2;
        self.expr_allowed = false;
        Some(self.token(Kind::Punct, start))
      }
      _ => {
        self.pos += 1;
        self.expr_allowed = true;
        Some(self.token(Kind::Punct, start))
      }
    }
  }

  fn scan_ident(&mut self, start: usize, after_dot: bool) -> Token<'a> {
    self.pos += 1;
    while self.byte_at(0).is_some_and(is_ident_continue) {
      self.pos += 1;
    }
    let word = &self.src[start..self.pos];
    self.expr_allowed = !after_dot && expression_after(word);
    self.prev_control = !after_dot && control_paren(word);
    self.block_expected =
      !after_dot && matches!(word, "do" | "else" | "finally" | "try");
    self.token(Kind::Ident, start)
  }

  fn scan_number(&mut self) {
    self.pos += 1;
    while let Some(next) = self.byte_at(0) {
      let exponent_sign = matches!(next, b'+' | b'-')
        && matches!(self.bytes.get(self.pos - 1), Some(b'e' | b'E'));
      if next.is_ascii_alphanumeric()
        || next == b'_'
        || next == b'.'
        || exponent_sign
      {
        self.pos += 1;
      } else {
        break;
      }
    }
  }

  fn slash(&mut self, start: usize) -> Token<'a> {
    // `/=` can open a regex (`/=x/`), so the expression context decides first.
    if self.expr_allowed
      && let Some(end) = self.scan_regex()
    {
      self.pos = end;
      self.expr_allowed = false;
      return self.token(Kind::Regex, start);
    }
    self.pos += if self.byte_at(1) == Some(b'=') { 2 } else { 1 };
    self.expr_allowed = true;
    self.token(Kind::Punct, start)
  }

  fn close_brace(&mut self) -> Option<Token<'a>> {
    let start = self.pos;
    let brace = self.braces.pop();
    self.pos += 1;
    if brace == Some(Brace::Template) {
      self.expr_allowed = false;
      if self.template_text() {
        self.braces.push(Brace::Template);
        self.expr_allowed = true;
      }
      return None;
    }
    self.expr_allowed = brace != Some(Brace::Object);
    Some(self.token(Kind::Punct, start))
  }

  fn scan_string(&mut self, quote: u8) {
    self.pos += 1;
    while let Some(byte) = self.byte_at(0) {
      match byte {
        b'\\' => self.pos = (self.pos + 2).min(self.bytes.len()),
        b'\n' => {
          self.pos += 1;
          return;
        }
        _ if byte == quote => {
          self.pos += 1;
          return;
        }
        _ => self.pos += 1,
      }
    }
  }

  /// Consumes template text; `true` when it stopped at `${` and an expression follows.
  fn template_text(&mut self) -> bool {
    while let Some(byte) = self.byte_at(0) {
      match byte {
        b'\\' => self.pos = (self.pos + 2).min(self.bytes.len()),
        b'`' => {
          self.pos += 1;
          return false;
        }
        b'$' if self.byte_at(1) == Some(b'{') => {
          self.pos += 2;
          return true;
        }
        _ => self.pos += 1,
      }
    }
    false
  }

  fn scan_regex(&self) -> Option<usize> {
    let mut index = self.pos + 1;
    let mut in_class = false;
    while let Some(&byte) = self.bytes.get(index) {
      match byte {
        b'\\' => index += 2,
        b'\n' | b'\r' => return None,
        b'[' => {
          in_class = true;
          index += 1;
        }
        b']' => {
          in_class = false;
          index += 1;
        }
        b'/' if !in_class => {
          index += 1;
          while self
            .bytes
            .get(index)
            .is_some_and(|byte| is_ident_continue(*byte))
          {
            index += 1;
          }
          return Some(index);
        }
        _ => index += 1,
      }
    }
    None
  }
}

impl<'a> Iterator for Lexer<'a> {
  type Item = Event<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      if let Some(comment) = self.skip_trivia() {
        return Some(Event::Comment(comment));
      }
      if self.pos >= self.bytes.len() {
        return None;
      }
      if let Some(token) = self.next_token() {
        return Some(Event::Token(token));
      }
    }
  }
}

/// Every significant token in `src`, without comments.
pub(crate) fn token_events(src: &str) -> impl Iterator<Item = Token<'_>> {
  Lexer::new(src).filter_map(|event| match event {
    Event::Token(token) => Some(token),
    Event::Comment(_) => None,
  })
}

/// Every significant token in `src`, without comments.
pub(crate) fn tokens(src: &str) -> Vec<Token<'_>> {
  token_events(src).collect()
}

const fn is_ident_start(byte: u8) -> bool {
  byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' || byte >= 0x80
}

const fn is_ident_continue(byte: u8) -> bool {
  is_ident_start(byte) || byte.is_ascii_digit()
}

/// Keywords that end in expression position, so a `/` after them starts a regex.
fn expression_after(word: &str) -> bool {
  matches!(
    word,
    "await"
      | "case"
      | "default"
      | "delete"
      | "do"
      | "else"
      | "extends"
      | "in"
      | "instanceof"
      | "new"
      | "of"
      | "return"
      | "throw"
      | "typeof"
      | "void"
      | "yield"
  )
}

/// Keywords whose parenthesized head is a statement, not an expression.
fn control_paren(word: &str) -> bool {
  matches!(word, "if" | "while" | "for" | "with")
}
