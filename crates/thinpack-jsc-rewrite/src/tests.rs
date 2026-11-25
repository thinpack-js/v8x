use crate::lexer::tokens;
use crate::rewrite::{binding_names, strip_comments};
use crate::{
  has_top_level_await, import_offsets, rewrite_dynamic_import_calls,
  rewrite_es_module,
};

fn exported_names(source: &str) -> Vec<String> {
  let mut names = rewrite_es_module(source)
    .expect("source rewrites")
    .export_names;
  names.sort();
  names
}

fn rewritten_body(source: &str) -> String {
  rewrite_es_module(source).expect("source rewrites").body
}

fn line_of(source: &str, offset: usize) -> usize {
  source[..offset]
    .bytes()
    .filter(|byte| *byte == b'\n')
    .count()
    + 1
}

fn specifiers(values: &[&str]) -> Vec<String> {
  values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn import_meta_is_replaced_only_as_syntax() {
  let source = r#"const text = "import.meta";
const template = `import.meta ${import.meta.url}`;
const pattern = /import\.meta/;
// import.meta in a comment
const spaced = import . meta;
"#;
  let body = rewritten_body(source);
  assert!(body.contains(r#"const text = "import.meta";"#));
  assert!(body.contains("const template = `import.meta ${__v8jsc_meta.url}`;"));
  assert!(body.contains(r"const pattern = /import\.meta/;"));
  assert!(body.contains("const spaced = __v8jsc_meta;"));
}

#[test]
fn regex_literals_after_control_parentheses_are_not_divisions() {
  let source =
    "if (ready) /[\"']/.test(text); // trailing\nconst ratio = (a) / b / c;\n";
  let stripped = strip_comments(source);
  assert!(stripped.contains("if (ready) /[\"']/.test(text);"));
  assert!(!stripped.contains("trailing"));
  assert!(stripped.contains("const ratio = (a) / b / c;"));
}

#[test]
fn regex_literals_after_a_block_are_not_divisions() {
  let source = "if (ready) {} /[\"']/.test(text); // trailing\nconst f = () => {}; /[/]/.test(text);\n";
  let stripped = strip_comments(source);
  assert!(stripped.contains("if (ready) {} /[\"']/.test(text);"));
  assert!(stripped.contains("const f = () => {}; /[/]/.test(text);"));
  assert!(!stripped.contains("trailing"));
}

#[test]
fn slash_equals_is_a_regex_only_in_expression_position() {
  let source = "const tag = /=import.meta/;\nlet count = 8; count /= 2; const ratio = count / 2;\n";
  assert_eq!(rewritten_body(source), source);

  let ts = tokens("count /= 2;");
  assert!(ts[1].is_punct("/="));
}

#[test]
fn block_comments_keep_their_line_breaks() {
  let source = "/* one\ntwo */\nconst value = 1;\n";
  let stripped = strip_comments(source);
  assert_eq!(stripped.lines().count(), source.lines().count());
  assert!(stripped.contains("\nconst value = 1;"));
}

#[test]
fn destructuring_exports_every_binding() {
  assert_eq!(
    exported_names(
      "export const { a: { b }, c = { d: 1 }, e: [f, ...g] } = o;"
    ),
    specifiers(&["b", "c", "f", "g"])
  );
  assert_eq!(
    exported_names("export const [x, , y = 2, ...rest] = values;"),
    specifiers(&["rest", "x", "y"])
  );
  assert_eq!(
    exported_names("export const { [key]: value, other } = o;"),
    specifiers(&["other", "value"])
  );
}

#[test]
fn binding_patterns_ignore_computed_keys_and_defaults() {
  let names = binding_names("{ [key]: value, other = { nested: 1 }, ...rest }");
  assert_eq!(names, specifiers(&["value", "other", "rest"]));
}

#[test]
fn identical_imports_keep_their_own_line() {
  let source = "import { a } from \"./m\";\n\nimport { b } from \"./m\";\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert_eq!(
    rewritten.import_checks,
    vec![
      ("./m".to_owned(), "a".to_owned(), 1),
      ("./m".to_owned(), "b".to_owned(), 3),
    ]
  );
}

#[test]
fn import_locations_skip_comment_lines() {
  let source = "// import { a } from \"./m\";\nimport { a } from \"./m\";\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert_eq!(rewritten.import_checks[0].2, 2);
}

#[test]
fn specifier_offsets_skip_comments_and_strings() {
  let source = "// import { fake } from \"./m\";\nconst text = \"./m\";\nimport { a } from './m';\nimport { b } from \"./m\";\n";
  let offsets = import_offsets(source, &specifiers(&["./m", "./m"]));
  assert_eq!(offsets.len(), 2);
  assert_eq!(line_of(source, offsets[0]), 3);
  assert_eq!(line_of(source, offsets[1]), 4);
  assert!(source[offsets[0]..].starts_with("'./m'"));
  assert!(source[offsets[1]..].starts_with("\"./m\""));
}

#[test]
fn a_joined_import_keeps_its_specifier_verbatim() {
  let source = "import {\n  a\n} from \"./a  b.js\";\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert_eq!(rewritten.imports, specifiers(&["./a  b.js"]));
}

#[test]
fn default_and_namespace_imports_bind_both_names() {
  let source = "import main, * as ns from \"./m\";\nexport { main, ns };\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert!(rewritten.body.contains(
    "const main = ((globalThis.__v8jsc_modules||{})[\"./m\"]||{}).default;"
  ));
  assert!(
    rewritten
      .body
      .contains("const ns = ((globalThis.__v8jsc_modules||{})[\"./m\"]||{});")
  );
  assert_eq!(rewritten.export_names, specifiers(&["main", "ns"]));
  assert_eq!(
    rewritten.reexports,
    vec![
      ("main".to_owned(), "./m".to_owned(), "default".to_owned()),
      ("ns".to_owned(), "./m".to_owned(), "*".to_owned()),
    ]
  );
}

#[test]
fn export_forms_work_without_a_space() {
  assert_eq!(
    exported_names("export default{a: 1};"),
    specifiers(&["default"])
  );
  assert_eq!(
    exported_names("export function* generate() { yield 1; }"),
    specifiers(&["generate"])
  );
  assert_eq!(
    exported_names("export class Widget {}"),
    specifiers(&["Widget"])
  );
  assert_eq!(
    exported_names("export const value = 1;"),
    specifiers(&["value"])
  );
}

#[test]
fn star_and_named_reexports_are_recorded() {
  let source = "export * from \"./a\";\nexport * as ns from \"./b\";\nexport { x as y } from \"./c\";\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert_eq!(rewritten.imports, specifiers(&["./a", "./b", "./c"]));
  assert!(rewritten.has_star_reexports);
  assert!(rewritten.export_names.contains(&"ns".to_owned()));
  assert!(rewritten.export_names.contains(&"y".to_owned()));
  assert_eq!(
    rewritten.import_checks,
    vec![("./c".to_owned(), "x".to_owned(), 3)]
  );
  assert_eq!(
    rewritten.reexports,
    vec![("y".to_owned(), "./c".to_owned(), "x".to_owned())]
  );
  assert!(rewritten.body.contains("Object.defineProperty(__ns,__k"));
}

#[test]
fn semicolons_in_strings_do_not_split_statements() {
  let source =
    "const text = \"a; export const hidden = 1\"; export const visible = 2;\n";
  let rewritten = rewrite_es_module(source).expect("source rewrites");
  assert_eq!(rewritten.export_names, specifiers(&["visible"]));
  assert!(
    rewritten
      .body
      .contains("const text = \"a; export const hidden = 1\";")
  );
}

#[test]
fn dynamic_import_rewrite_ignores_text_and_properties() {
  let source = "const text = \"import(\\\"x\\\")\";\n// import(\"y\")\nconst template = `import(\"z\")`;\nobj.import(\"method\");\nimport(\"real\");\n";
  let rewritten = rewrite_dynamic_import_calls(source);
  assert!(rewritten.contains("const text = \"import(\\\"x\\\")\";"));
  assert!(rewritten.contains("const template = `import(\"z\")`;"));
  assert!(rewritten.contains("obj.import(\"method\");"));
  assert!(rewritten.contains("__v82jsc_dynImport(\"real\");"));
}

#[test]
fn top_level_await_is_detected_where_it_occurs() {
  assert!(has_top_level_await("await setup();"));
  assert!(has_top_level_await("const value = `${await read()}`;"));
  assert!(has_top_level_await("for await (const item of items) {}"));
  assert!(!has_top_level_await("const f = async () => await read();"));
  assert!(!has_top_level_await("async function f() { await read(); }"));
  assert!(!has_top_level_await("const text = \"await read()\";"));
  assert!(!has_top_level_await("const pattern = /await read/;"));
  assert!(!has_top_level_await("item.await(count);"));
}

#[test]
fn an_export_star_without_a_specifier_is_unsupported() {
  assert!(rewrite_es_module("export *;").is_none());
}
