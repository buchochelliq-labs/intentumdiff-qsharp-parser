//! Q# (Microsoft Quantum) parser plugin — full-parse mode.
//!
//! Handles `.qs` files with a small lexer (strings, interpolated strings and
//! `//` comments) and a brace-structured recursive parser. No tree-sitter
//! grammar is used.
//!
//! Semantic nodes produced (labels are source tokens with layout/comments
//! collapsed to single spaces):
//!   compilation_unit — root
//!   namespace        — namespace Foo { … } (label = namespace name)
//!   operation        — operation Bar(…) : T { … } (label = name; first child qs_signature)
//!   function         — function Baz(…) : T { … } (label = name; first child qs_signature)
//!   struct           — struct S { … } (label = name; first child qs_signature)
//!   newtype          — newtype Complex = (…); (label = name; child qs_signature)
//!   qs_block         — any other `head { … }` (if/else/for/within/apply/specializations)
//!   qs_statement     — any `…;` statement or trailing expression
//!
//! Unbalanced delimiters or unterminated strings return the in-band
//! `{"error": …}` envelope rather than a recovered tree.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct QSharpParser;

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

pub(crate) fn detect_language_impl(filename: &str, _content: &str) -> String {
    if filename.to_lowercase().ends_with(".qs") {
        "qsharp".to_string()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------
//
// The projection is token based (not line based) so that:
//   * declarations and bodies on the same line (`function F() : Int { return 1; }`)
//     are all retained (issue #8);
//   * `//` inside a quoted string (`"https://a.example"`) is string content, not a
//     comment (issue #8);
//   * braces inside strings / interpolations never disturb the structure, and
//     unbalanced delimiters or unterminated strings produce an explicit in-band
//     error instead of a recovered tree with clashing node IDs (issue #9).

/// Maximum brace nesting the parser descends into before giving up with an
/// in-band error (bounded recursion inside the Wasm sandbox).
const MAX_NESTING: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Word,
    Str,
    Punct,
}

struct Token<'a> {
    kind: TokKind,
    text: &'a str,
    /// Byte offsets into the source.
    start: usize,
    end: usize,
    /// Whitespace or a comment separated this token from the previous one.
    spaced: bool,
}

impl Token<'_> {
    fn is(&self, punct: &str) -> bool {
        self.kind == TokKind::Punct && self.text == punct
    }
    fn word(&self) -> Option<&str> {
        (self.kind == TokKind::Word).then_some(self.text)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// Scan a string literal starting at `start` (the opening `"`). For an
/// interpolated string (`$"…{expr}…"`) braces nest and may contain plain or
/// interpolated string literals. Returns the byte offset just past the
/// closing quote.
fn scan_string(src: &str, start: usize, interpolated: bool) -> Result<usize, String> {
    scan_string_nested(src, start, interpolated, 0)
}

fn scan_string_nested(
    src: &str,
    start: usize,
    interpolated: bool,
    nesting: usize,
) -> Result<usize, String> {
    if nesting > MAX_NESTING {
        return Err(format!(
            "interpolated strings nested deeper than {MAX_NESTING} levels"
        ));
    }
    let bytes = src.as_bytes();
    let mut i = start + 1;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' if depth == 0 => return Ok(i + 1),
            // A string inside an interpolation hole: `$"` keeps
            // interpolation mode so its own holes balance (#11).
            b'"' => {
                let nested = bytes[i - 1] == b'$';
                i = scan_string_nested(src, i, nested, nesting + 1)?;
            }
            b'{' if interpolated => {
                depth += 1;
                i += 1;
            }
            b'}' if interpolated && depth > 0 => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Err(format!(
        "unterminated string literal starting at byte {start}"
    ))
}

fn tokenize(src: &str) -> Result<Vec<Token<'_>>, String> {
    let mut tokens = Vec::new();
    let mut spaced = false;
    let mut iter = src.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if c.is_whitespace() {
            spaced = true;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("//") {
            // Line comment (including `///` doc comments): trivia up to newline.
            while let Some(&(_, n)) = iter.peek() {
                if n == '\n' {
                    break;
                }
                iter.next();
            }
            spaced = true;
            continue;
        }
        let (kind, end) = if c == '"' {
            (TokKind::Str, scan_string(src, i, false)?)
        } else if c == '$' && rest[1..].starts_with('"') {
            (TokKind::Str, scan_string(src, i + 1, true)?)
        } else if is_word_char(c) {
            let mut end = i + c.len_utf8();
            while let Some(&(j, n)) = iter.peek() {
                if !is_word_char(n) {
                    break;
                }
                end = j + n.len_utf8();
                iter.next();
            }
            (TokKind::Word, end)
        } else {
            (TokKind::Punct, i + c.len_utf8())
        };
        // Skip the characters a string scan consumed.
        while let Some(&(j, _)) = iter.peek() {
            if j >= end {
                break;
            }
            iter.next();
        }
        tokens.push(Token { kind, text: &src[i..end], start: i, end, spaced });
        spaced = false;
    }
    Ok(tokens)
}

/// Canonical text of a token run: source tokens verbatim, any whitespace or
/// comment between two tokens collapsed to one space. Layout and comments are
/// therefore insignificant while every operator, literal and name is kept.
fn normalized_text(tokens: &[Token<'_>]) -> String {
    let mut out = String::new();
    for (n, t) in tokens.iter().enumerate() {
        if n > 0 && t.spaced {
            out.push(' ');
        }
        out.push_str(t.text);
    }
    out
}

// ---------------------------------------------------------------------------
// Structural parser
// ---------------------------------------------------------------------------

struct Item {
    node_type: &'static str,
    label: String,
    start: usize,
    end: usize,
    children: Vec<Item>,
}

impl Item {
    fn leaf(node_type: &'static str, tokens: &[Token<'_>]) -> Item {
        Item {
            node_type,
            label: normalized_text(tokens),
            start: tokens.first().map_or(0, |t| t.start),
            end: tokens.last().map_or(0, |t| t.end),
            children: Vec::new(),
        }
    }
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    pos: usize,
}

/// Index of the declaration keyword in `head`, skipping attributes
/// (`@EntryPoint()`) and access modifiers.
fn declaration_keyword(head: &[Token<'_>]) -> Option<(usize, &'static str)> {
    let mut j = 0;
    while j < head.len() {
        if head[j].is("@") {
            j += 2; // `@` + attribute name
            if head.get(j).is_some_and(|t| t.is("(")) {
                let mut depth = 0i32;
                while j < head.len() {
                    if head[j].is("(") {
                        depth += 1;
                    } else if head[j].is(")") {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    j += 1;
                }
            }
            continue;
        }
        return match head[j].word() {
            Some("internal") => {
                j += 1;
                continue;
            }
            Some("namespace") => Some((j, "namespace")),
            Some("operation") => Some((j, "operation")),
            Some("function") => Some((j, "function")),
            Some("newtype") => Some((j, "newtype")),
            Some("struct") => Some((j, "struct")),
            _ => None,
        };
    }
    None
}

fn declaration_name(head: &[Token<'_>], kw: usize) -> String {
    head.get(kw + 1)
        .and_then(Token::word)
        .map_or_else(|| "(anonymous)".to_string(), str::to_string)
}

impl Parser<'_> {
    /// Parse items until EOF (`nested == false`) or the matching `}`.
    /// Returns the items and, for a nested scope, the closing brace's end offset.
    fn scope(&mut self, depth: usize) -> Result<(Vec<Item>, usize), String> {
        if depth > MAX_NESTING {
            return Err(format!("braces nested deeper than {MAX_NESTING} levels"));
        }
        let nested = depth > 0;
        let mut items = Vec::new();
        loop {
            let Some(tok) = self.tokens.get(self.pos) else {
                if nested {
                    return Err("unclosed '{' at end of input".to_string());
                }
                return Ok((items, 0));
            };
            if tok.is("}") {
                if !nested {
                    return Err(format!("unmatched '}}' at byte {}", tok.start));
                }
                let end = tok.end;
                self.pos += 1;
                return Ok((items, end));
            }
            if tok.is(";") {
                self.pos += 1;
                continue;
            }
            items.push(self.item(depth)?);
        }
    }

    /// Parse one statement, declaration or block starting at `self.pos`.
    fn item(&mut self, depth: usize) -> Result<Item, String> {
        let head_start = self.pos;
        let mut open: Vec<&str> = Vec::new();
        let terminator = loop {
            let Some(tok) = self.tokens.get(self.pos) else {
                if let Some(o) = open.last() {
                    return Err(format!("unclosed '{o}' at end of input"));
                }
                break None;
            };
            if tok.kind == TokKind::Punct {
                match tok.text {
                    "(" | "[" => open.push(tok.text),
                    "{" if !open.is_empty() => open.push("{"),
                    ")" | "]" | "}" if !open.is_empty() => {
                        let want = match tok.text {
                            ")" => "(",
                            "]" => "[",
                            _ => "{",
                        };
                        if open.pop() != Some(want) {
                            return Err(format!(
                                "mismatched '{}' at byte {}",
                                tok.text, tok.start
                            ));
                        }
                    }
                    ")" | "]" => {
                        return Err(format!("unmatched '{}' at byte {}", tok.text, tok.start));
                    }
                    ";" | "{" | "}" => break Some(tok.text),
                    _ => {}
                }
            }
            self.pos += 1;
        };
        let head_end = self.pos;
        let head = &self.tokens[head_start..head_end];
        let keyword = declaration_keyword(head);

        if terminator == Some("{") {
            self.pos += 1; // consume `{`
            let (body, close_end) = self.scope(depth + 1)?;
            let head = &self.tokens[head_start..head_end];
            let start = head.first().map_or(self.tokens[head_end].start, |t| t.start);
            let item = match keyword {
                Some((kw, "namespace")) => Item {
                    node_type: "namespace",
                    label: declaration_name(head, kw),
                    start,
                    end: close_end,
                    children: body,
                },
                Some((kw, node_type)) => {
                    let mut children = vec![Item::leaf("qs_signature", head)];
                    children.extend(body);
                    Item {
                        node_type,
                        label: declaration_name(head, kw),
                        start,
                        end: close_end,
                        children,
                    }
                }
                None => Item {
                    node_type: "qs_block",
                    label: if head.is_empty() {
                        "{}".to_string()
                    } else {
                        normalized_text(head)
                    },
                    start,
                    end: close_end,
                    children: body,
                },
            };
            return Ok(item);
        }

        if terminator == Some(";") {
            self.pos += 1; // consume `;`
        }
        // A `}` terminator is left for the enclosing scope (trailing
        // expression without `;`, e.g. an implicit return value).
        Ok(match keyword {
            Some((kw, node_type)) if node_type != "namespace" => Item {
                node_type,
                label: declaration_name(head, kw),
                start: head[0].start,
                end: head[head.len() - 1].end,
                children: vec![Item::leaf("qs_signature", head)],
            },
            _ => Item::leaf("qs_statement", head),
        })
    }
}

// ---------------------------------------------------------------------------
// SemanticNode conversion
// ---------------------------------------------------------------------------

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(src: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { starts }
    }
    /// 0-based line and UTF-8 byte column of a byte offset.
    fn position(&self, offset: usize) -> (u32, u32) {
        let line = self.starts.partition_point(|&s| s <= offset).saturating_sub(1);
        (line as u32, (offset - self.starts[line]) as u32)
    }
}

/// IDs are allocated from each node's final position among its parent's
/// children, so they are unique by construction for any input.
fn to_node(item: Item, id: &str, lines: &LineIndex) -> SemanticNode {
    let (start_line, start_col) = lines.position(item.start);
    let (end_line, end_col) = lines.position(item.end);
    let children = item
        .children
        .into_iter()
        .enumerate()
        .map(|(i, child)| to_node(child, &format!("{id}.{i}"), lines))
        .collect();
    SemanticNodeBuilder::new(
        id, item.node_type, item.label, start_line, start_col, end_line, end_col,
        String::new(),
    )
    .children(children)
    .build()
}

fn error_json(message: &str) -> String {
    serde_json::json!({ "error": format!("qsharp parse error: {message}") }).to_string()
}

pub(crate) fn parse_qsharp(source: &str) -> String {
    let tokens = match tokenize(source) {
        Ok(tokens) => tokens,
        Err(e) => return error_json(&e),
    };
    let mut parser = Parser { tokens, pos: 0 };
    let items = match parser.scope(0) {
        Ok((items, _)) => items,
        Err(e) => return error_json(&e),
    };
    let root = Item {
        node_type: "compilation_unit",
        label: "compilation_unit".to_string(),
        start: 0,
        end: source.len(),
        children: items,
    };
    let lines = LineIndex::new(source);
    match serde_json::to_string(&to_node(root, "0", &lines)) {
        Ok(s) => s,
        Err(e) => error_json(&format!("serialisation error: {e}")),
    }
}

// ---------------------------------------------------------------------------
// WIT guest impl
// ---------------------------------------------------------------------------

impl Guest for QSharpParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "qsharp".to_string()
    }
    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "namespace Demo {\n    open Microsoft.Quantum.Intrinsic;\n\n    operation SayHello() : Unit {\n        Message(\"Hello, World!\");\n    }\n}\n".to_string(),
            new: "namespace Demo {\n    open Microsoft.Quantum.Intrinsic;\n    open Microsoft.Quantum.Canon;\n\n    operation SayHello(name : String) : Unit {\n        Message($\"Hello, {name}!\");\n    }\n\n    operation FlipBit() : Result {\n        use q = Qubit();\n        X(q);\n        let result = M(q);\n        Reset(q);\n        return result;\n    }\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        parse_qsharp(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["qsharp".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(QSharpParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    intentumdiff_plugin_sdk::plugin_compliance_tests! {
        process: parse_qsharp,
        detect_fn: detect_language_impl,
        detect_cases: [
            ("Bell.qs",   "", "qsharp"),
            ("Bell.QS",   "", "qsharp"),
            ("main.rs",   "", ""),
            ("main.cs",   "", ""),
        ],
        grammar_id: "qsharp",
        language_ids: ["qsharp"],
    }

    const SAMPLE: &str = "namespace MyQuantum {\n\
        open Microsoft.Quantum.Intrinsic;\n\
        \n\
        operation Bell(q1 : Qubit, q2 : Qubit) : Unit {\n\
            H(q1);\n\
            CNOT(q1, q2);\n\
        }\n\
        \n\
        function Square(n : Int) : Int {\n\
            return n * n;\n\
        }\n\
        \n\
        newtype Complex = (Real : Double, Imag : Double);\n\
    }";

    #[test]
    fn test_valid_json_no_error() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&out, "SAMPLE");
        intentumdiff_plugin_sdk::testing::assert_no_error(&out, "SAMPLE");
    }

    #[test]
    fn test_root_is_compilation_unit() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&out, "compilation_unit", "SAMPLE");
    }

    #[test]
    fn test_namespace_found() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "namespace", "namespace");
    }

    #[test]
    fn test_operation_found() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "operation", "operation");
    }

    #[test]
    fn test_function_found() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "function", "function");
    }

    #[test]
    fn test_newtype_found() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(&out, "newtype", "newtype");
    }

    #[test]
    fn operation_parameter_change_is_semantic() {
        let before = "namespace Demo {\n    operation SayHello() : Unit {\n        Message(\"Hello, World!\");\n    }\n}";
        let after = "namespace Demo {\n    operation SayHello(name : String) : Unit {\n        Message(\"Hello, World!\");\n    }\n}";

        let before_tree = parse_qsharp(before);
        let after_tree = parse_qsharp(after);

        assert_ne!(before_tree, after_tree, "operation signature change must alter the semantic tree");
        assert!(
            after_tree.contains("qs_signature") && after_tree.contains("name : String"),
            "operation signature evidence must be retained: {after_tree}"
        );
    }

    #[test]
    fn test_changed_operation_signature_anchors_exact_source_token() {
        let source = "namespace Demo {\n    operation SayHello(name : String) : Unit {\n        Message(\"Hello\");\n    }\n}\n";
        let parsed: serde_json::Value = serde_json::from_str(&parse_qsharp(source)).unwrap();
        let signature = &parsed["children"][0]["children"][0]["children"][0];
        assert_eq!(signature["node_type"], "qs_signature");
        assert_eq!(signature["position"]["start_line"], 1);
        assert_eq!(signature["position"]["end_line"], 1);
        assert_eq!(signature["position"]["start_col"], 4);
        let label = signature["label"].as_str().unwrap();
        assert_eq!(label, "operation SayHello(name : String) : Unit");
        assert_eq!(signature["position"]["end_col"], 4 + label.len());
        let source_line = source.lines().nth(1).unwrap();
        let changed_col = source_line.find("name : String").unwrap();
        assert!(4 <= changed_col);
        assert!(4 + label.len() >= changed_col + "name : String".len());
    }

    #[test]
    fn test_indented_function_and_statement_spans_cover_source() {
        let source = "namespace Demo {\n    function Twice(n : Int) : Int {\n        return n + n;\n    }\n}\n";
        let parsed: serde_json::Value = serde_json::from_str(&parse_qsharp(source)).unwrap();
        let function = &parsed["children"][0]["children"][0];
        let signature = &function["children"][0];
        let statement = &function["children"][1];
        assert_eq!(signature["node_type"], "qs_signature");
        assert_eq!(signature["position"]["start_line"], 1);
        assert_eq!(signature["position"]["start_col"], 4);
        assert_eq!(
            signature["position"]["end_col"],
            4 + "function Twice(n : Int) : Int".len(),
        );
        assert_eq!(statement["node_type"], "qs_statement");
        assert_eq!(statement["label"], "return n + n");
        assert_eq!(statement["position"]["start_line"], 2);
        assert_eq!(statement["position"]["start_col"], 8);
        assert_eq!(statement["position"]["end_col"], 8 + "return n + n".len());
    }

    #[test]
    fn test_internal_operation() {
        let src = "namespace Lib {\n    internal operation Private() : Unit {\n        let x = 1;\n    }\n}";
        let out = parse_qsharp(src);
        intentumdiff_plugin_sdk::testing::assert_contains_node_type(
            &out,
            "operation",
            "internal operation",
        );
    }

    #[test]
    fn test_labels_nonempty() {
        let out = parse_qsharp(SAMPLE);
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "namespace", "labels");
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "operation", "labels");
        intentumdiff_plugin_sdk::testing::assert_labels_nonempty(&out, "function", "labels");
    }

    // -----------------------------------------------------------------------
    // Regression helpers
    // -----------------------------------------------------------------------

    fn tree(src: &str) -> serde_json::Value {
        let out = parse_qsharp(src);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_none(), "unexpected error for {src:?}: {out}");
        v
    }

    fn root_hash(src: &str) -> String {
        tree(src)["structural_hash"].as_str().unwrap().to_string()
    }

    fn find_label<'a>(node: &'a serde_json::Value, node_type: &str, label: &str) -> Option<&'a serde_json::Value> {
        if node["node_type"] == node_type && node["label"] == label {
            return Some(node);
        }
        node["children"].as_array()?.iter().find_map(|c| find_label(c, node_type, label))
    }

    fn collect_ids(node: &serde_json::Value, out: &mut Vec<String>) {
        out.push(node["id"].as_str().unwrap().to_string());
        for c in node["children"].as_array().unwrap() {
            collect_ids(c, out);
        }
    }

    fn assert_unique_ids(v: &serde_json::Value) {
        let mut ids = Vec::new();
        collect_ids(v, &mut ids);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate ids: {ids:?}");
    }

    fn assert_semantic_change(old: &str, new: &str, node_type: &str, new_label: &str) {
        assert_ne!(root_hash(old), root_hash(new), "{old:?} -> {new:?} must change the tree");
        let t = tree(new);
        assert!(
            find_label(&t, node_type, new_label).is_some(),
            "missing {node_type} {new_label:?} in {t}"
        );
    }

    fn assert_error(src: &str) {
        let out = parse_qsharp(src);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").and_then(|e| e.as_str()).is_some(), "expected error envelope for {src:?}: {out}");
    }

    // -----------------------------------------------------------------------
    // Issue #8: inline bodies and `//` inside quoted strings
    // -----------------------------------------------------------------------

    #[test]
    fn issue8_inline_operation_body_literal() {
        assert_semantic_change(
            "operation F() : Int { return 1; }\n",
            "operation F() : Int { return 2; }\n",
            "qs_statement",
            "return 2",
        );
    }

    #[test]
    fn issue8_quoted_comment_marker_multiline() {
        assert_semantic_change(
            "operation F() : Unit {\n  Message(\"https://safe\");\n}\n",
            "operation F() : Unit {\n  Message(\"https://danger\");\n}\n",
            "qs_statement",
            "Message(\"https://danger\")",
        );
    }

    #[test]
    fn assessment_literal_value() {
        assert_semantic_change(
            "namespace Probe { function Answer() : Int { return 1; } }\n",
            "namespace Probe { function Answer() : Int { return 2; } }\n",
            "qs_statement",
            "return 2",
        );
    }

    #[test]
    fn assessment_declaration_api() {
        assert_semantic_change(
            "namespace Probe { function Answer() : Int { return 1; } }\n",
            "namespace Probe { function ResultValue() : Int { return 1; } }\n",
            "function",
            "ResultValue",
        );
    }

    #[test]
    fn assessment_body_operator() {
        assert_semantic_change(
            "namespace Probe { function Combine(a : Int, b : Int) : Int { return a + b; } }\n",
            "namespace Probe { function Combine(a : Int, b : Int) : Int { return a - b; } }\n",
            "qs_statement",
            "return a - b",
        );
    }

    #[test]
    fn assessment_unicode_payload_spans_are_utf8_bytes() {
        let new = "namespace Probe { function MessageText() : String { return \"cafe\u{96ea}\"; } }\n";
        assert_semantic_change(
            "namespace Probe { function MessageText() : String { return \"caf\u{e9}\"; } }\n",
            new,
            "qs_statement",
            "return \"cafe\u{96ea}\"",
        );
        let t = tree(new);
        let stmt = find_label(&t, "qs_statement", "return \"cafe\u{96ea}\"").unwrap();
        let start = new.find("return").unwrap();
        let end = new.find("\";").unwrap() + 1;
        assert_eq!(stmt["position"]["start_line"], 0);
        assert_eq!(stmt["position"]["start_col"], start);
        assert_eq!(stmt["position"]["end_col"], end);
    }

    #[test]
    fn assessment_quoted_comment_marker_inline() {
        let new = "namespace Probe { function Url() : String { return \"https://b.example\"; } }\n";
        assert_semantic_change(
            "namespace Probe { function Url() : String { return \"https://a.example\"; } }\n",
            new,
            "qs_statement",
            "return \"https://b.example\"",
        );
        // The brace after the string must still close the function and namespace.
        let t = tree(new);
        assert_eq!(t["children"].as_array().unwrap().len(), 1);
        assert_eq!(t["children"][0]["node_type"], "namespace");
        assert_eq!(t["children"][0]["children"][0]["node_type"], "function");
    }

    #[test]
    fn inline_and_multiline_bodies_have_same_semantics() {
        let inline = "namespace Probe { function Answer() : Int { return 1; } }\n";
        let multi = "namespace Probe {\n    function Answer() : Int {\n        return 1;\n    }\n}\n";
        assert_eq!(root_hash(inline), root_hash(multi));
    }

    #[test]
    fn comments_and_layout_are_insignificant() {
        let base = "namespace Probe { function Answer() : Int { return 1; } }\n";
        assert_eq!(root_hash(base), root_hash("// Ordinary comment\nnamespace Probe { function Answer() : Int { return 1; } }\n"));
        assert_eq!(root_hash(base), root_hash("\nnamespace Probe { function Answer() : Int { return 1; } }\n\n"));
        assert_eq!(
            root_hash(base),
            root_hash("namespace Probe { function Answer() : Int { return 1; // trailing } {\n } }\n")
        );
    }

    #[test]
    fn braces_in_interpolated_strings_do_not_affect_structure() {
        let src = "namespace Demo {\n    operation SayHello(name : String) : Unit {\n        Message($\"Hello, {name}! }}\");\n    }\n}\n";
        let t = tree(src);
        assert!(find_label(&t, "qs_statement", "Message($\"Hello, {name}! }}\")").is_some(), "{t}");
        assert_unique_ids(&t);
    }

    // Issue #11: a nested interpolated string keeps interpolation mode.
    #[test]
    fn nested_interpolated_strings_balance_their_holes() {
        let src = "operation F() : Unit { Message($\"{$\"{ \"}\" }\"}\"); }";
        let t = tree(src);
        let mut ops = Vec::new();
        fn collect(n: &serde_json::Value, ty: &str, out: &mut Vec<String>) {
            if n["node_type"] == ty {
                out.push(n["label"].as_str().unwrap().to_string());
            }
            for c in n["children"].as_array().into_iter().flatten() {
                collect(c, ty, out);
            }
        }
        collect(&t, "operation", &mut ops);
        assert_eq!(ops.len(), 1, "{t}");
        assert!(
            find_label(&t, "qs_statement", "Message($\"{$\"{ \"}\" }\"}\")").is_some(),
            "{t}"
        );
        assert_unique_ids(&t);
        // A plain nested `"}"` inside a hole is still a literal.
        let plain = "operation F() : Unit { Message($\"{ \"}\" }\"); }";
        assert!(
            find_label(&tree(plain), "qs_statement", "Message($\"{ \"}\" }\")").is_some()
        );
        // Deeply nested interpolations are bounded with an explicit error.
        let deep = format!(
            "operation F() : Unit {{ Message({}x{}); }}",
            "$\"{".repeat(400),
            "}\"".repeat(400)
        );
        assert_error(&deep);
    }

    #[test]
    fn nested_block_boundaries_are_structural() {
        let a = "operation F() : Unit { if c { X(q); } Y(q); }";
        let b = "operation F() : Unit { if c { X(q); Y(q); } }";
        assert_ne!(root_hash(a), root_hash(b));
        assert!(find_label(&tree(a), "qs_block", "if c").is_some());
    }

    #[test]
    fn newtype_definition_change_is_semantic() {
        assert_ne!(
            root_hash("newtype Complex = (Real : Double, Imag : Double);"),
            root_hash("newtype Complex = (Real : Double, Imag : Int);"),
        );
    }

    // -----------------------------------------------------------------------
    // Issue #9: unique IDs or explicit errors for malformed declarations
    // -----------------------------------------------------------------------

    #[test]
    fn issue9_fuzz_002_returns_error_envelope() {
        assert_error("namespace Demo {\n    open Microsoft.Quantum.Intrinsic;\n    open Microsoft.Quantum.Canon;\n\n    operation Sayllo, {name}!\");\n    }\n\n    operation FlipBit() : Result {\n        use q = Qubit();\n        X(q);\n        let result = M(q);\n        Reset(q);\n        return result;\n    }\n}\n");
    }

    #[test]
    fn issue9_malformed_inputs_error_explicitly() {
        assert_error("namespace Probe { function Answer(\n");
        assert_error("operation A() : Unit { }\n}\noperation B() : Unit { }\n");
        assert_error("operation A() : Unit { X(q]; }");
        assert_error("function F() : String { return \"open; }");
        assert_error(&"{".repeat(1000));
    }

    #[test]
    fn issue9_multiple_root_declarations_have_unique_ids() {
        let src = "operation A() : Unit { X(q); }\noperation B() : Unit { Y(q); }\nnewtype P = (Int, Int);\nnewtype Q = Int;\nfunction C() : Int { return \"}\" == \"{\" ? 1 | 2; }\n";
        let t = tree(src);
        assert_unique_ids(&t);
        assert_eq!(t["children"].as_array().unwrap().len(), 5);
    }

    // -----------------------------------------------------------------------
    // Issue #5: modified signature has a non-empty source span
    // -----------------------------------------------------------------------

    #[test]
    fn issue5_signature_span_covers_inserted_parameter() {
        let src = "namespace Demo {\n    open Microsoft.Quantum.Intrinsic;\n\n    operation SayHello(name : String) : Unit {\n        Message(\"Hello, World!\");\n    }\n}\n";
        let t = tree(src);
        let sig = find_label(&t, "qs_signature", "operation SayHello(name : String) : Unit").unwrap();
        let line = src.lines().nth(3).unwrap();
        assert_eq!(sig["position"]["start_line"], 3);
        assert_eq!(sig["position"]["end_line"], 3);
        assert_eq!(sig["position"]["start_col"], 4);
        assert_eq!(sig["position"]["end_col"], line.find(" {").unwrap());
        let op = find_label(&t, "operation", "SayHello").unwrap();
        assert_eq!(op["position"]["start_line"], 3);
        assert_eq!(op["position"]["start_col"], 4);
        assert_eq!(op["position"]["end_line"], 5);
        assert_eq!(op["position"]["end_col"], 5);
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(parse_qsharp(SAMPLE), parse_qsharp(SAMPLE));
    }
}
