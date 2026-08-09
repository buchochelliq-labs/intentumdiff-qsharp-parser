//! Q# (Microsoft Quantum) parser plugin — full-parse mode.
//!
//! Handles `.qs` files using brace-depth tracking.
//! No tree-sitter grammar is used; this plugin scans for declaration keywords.
//!
//! Semantic nodes produced:
//!   compilation_unit — root
//!   namespace        — namespace Foo { … } (label = namespace name)
//!   operation        — operation Bar(…) : T { … } (label = operation name)
//!   function         — function Baz(…) : T { … } (label = function name)
//!   newtype          — newtype Complex = (…) (label = type name, leaf)

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
// Node helpers
// ---------------------------------------------------------------------------

fn leaf(id: &str, node_type: &str, label: &str, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, line, 0, line, 0, String::new()).build()
}

fn block_node(
    id: &str,
    node_type: &str,
    label: &str,
    start: u32,
    end: u32,
    children: Vec<SemanticNode>,
) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label, start, 0, end, 0, String::new())
        .children(children)
        .build()
}

// ---------------------------------------------------------------------------
// Stack frame
// ---------------------------------------------------------------------------

struct QsFrame {
    id: String,
    node_type: &'static str,
    label: String,
    start_line: u32,
    /// The frame closes when brace_depth falls to or below this value.
    close_depth: i32,
    children: Vec<SemanticNode>,
}

fn push_to(stack: &mut Vec<QsFrame>, root: &mut Vec<SemanticNode>, node: SemanticNode) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        root.push(node);
    }
}

// ---------------------------------------------------------------------------
// Name extraction
// ---------------------------------------------------------------------------

/// Find `keyword` (case-insensitive) in `trimmed`, then return the
/// next whitespace-delimited token, stopping at `(`, `<`, `{`, `:`.
fn extract_qs_name(trimmed: &str, keyword: &str) -> String {
    let lower = trimmed.to_lowercase();
    let kw_lower = keyword.to_lowercase();
    let pos = match lower.find(&*kw_lower) {
        Some(p) => p + kw_lower.len(),
        None => return "(anonymous)".to_string(),
    };
    let rest = trimmed[pos..].trim_start();
    let end = rest
        .find(|c: char| c == '(' || c == '<' || c == '{' || c == ':' || c.is_whitespace())
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        "(anonymous)".to_string()
    } else {
        name.to_string()
    }
}

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
// Parser
// ---------------------------------------------------------------------------

pub(crate) fn parse_qsharp(source: &str) -> String {
    let mut root_children: Vec<SemanticNode> = Vec::new();
    let mut stack: Vec<QsFrame> = Vec::new();
    let mut counter: usize = 0;
    let mut brace_depth: i32 = 0;
    let total_lines = source.lines().count().saturating_sub(1) as u32;

    for (idx, raw_line) in source.lines().enumerate() {
        let lineno = idx as u32;
        let trimmed = raw_line.trim();

        // Strip line comments
        let trimmed = if let Some(p) = trimmed.find("//") {
            trimmed[..p].trim()
        } else {
            trimmed
        };

        if trimmed.is_empty() {
            continue;
        }

        let depth_before = brace_depth;
        let open_count = trimmed.chars().filter(|&c| c == '{').count() as i32;
        let close_count = trimmed.chars().filter(|&c| c == '}').count() as i32;
        brace_depth += open_count - close_count;

        // Close frames whose close_depth has been reached
        loop {
            match stack.last() {
                Some(f) if f.close_depth >= brace_depth => {
                    let frame = stack.pop().unwrap();
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    push_to(&mut stack, &mut root_children, n);
                }
                _ => break,
            }
        }

        // Detect declarations
        let lower = trimmed.to_lowercase();
        let mut words = lower.split_whitespace();
        let first_word = words.next().unwrap_or("");
        let second_word = words.next().unwrap_or("");
        let keyword_word = if first_word == "internal" {
            second_word
        } else {
            first_word
        };

        match keyword_word {
            "namespace" => {
                let name = extract_qs_name(trimmed, "namespace");
                let id = format!("0.{}", counter);
                counter += 1;
                let close_depth = depth_before;
                stack.push(QsFrame {
                    id,
                    node_type: "namespace",
                    label: name,
                    start_line: lineno,
                    close_depth,
                    children: vec![],
                });
                // Same-line close (e.g. namespace Foo {})
                if open_count > 0 && brace_depth <= depth_before {
                    let frame = stack.pop().unwrap();
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    push_to(&mut stack, &mut root_children, n);
                }
            }
            "operation" => {
                let name = extract_qs_name(trimmed, "operation");
                let parent_id = stack
                    .last()
                    .map(|f| f.id.as_str())
                    .unwrap_or("0")
                    .to_string();
                let child_idx = stack.last().map(|f| f.children.len()).unwrap_or(0);
                let id = format!("{}.{}", parent_id, child_idx);
                let close_depth = depth_before;
                stack.push(QsFrame {
                    id,
                    node_type: "operation",
                    label: name,
                    start_line: lineno,
                    close_depth,
                    children: vec![],
                });
                if open_count > 0 && brace_depth <= depth_before {
                    let frame = stack.pop().unwrap();
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    push_to(&mut stack, &mut root_children, n);
                }
            }
            "function" => {
                let name = extract_qs_name(trimmed, "function");
                let parent_id = stack
                    .last()
                    .map(|f| f.id.as_str())
                    .unwrap_or("0")
                    .to_string();
                let child_idx = stack.last().map(|f| f.children.len()).unwrap_or(0);
                let id = format!("{}.{}", parent_id, child_idx);
                let close_depth = depth_before;
                stack.push(QsFrame {
                    id,
                    node_type: "function",
                    label: name,
                    start_line: lineno,
                    close_depth,
                    children: vec![],
                });
                if open_count > 0 && brace_depth <= depth_before {
                    let frame = stack.pop().unwrap();
                    let n = block_node(
                        &frame.id,
                        frame.node_type,
                        &frame.label,
                        frame.start_line,
                        lineno,
                        frame.children,
                    );
                    push_to(&mut stack, &mut root_children, n);
                }
            }
            "newtype" => {
                let name = extract_qs_name(trimmed, "newtype");
                let parent_id = stack
                    .last()
                    .map(|f| f.id.as_str())
                    .unwrap_or("0")
                    .to_string();
                let child_idx = stack.last().map(|f| f.children.len()).unwrap_or(0);
                let id = format!("{}.{}", parent_id, child_idx);
                let n = leaf(&id, "newtype", &name, lineno);
                push_to(&mut stack, &mut root_children, n);
            }
            _ => {
                // #46: interior statement lines are review content — operation bodies were
                // empty shells, so a string edit inside Message(...) hashed style-only.
                let stmt = trimmed.trim_end_matches(';').trim();
                let bare_brace = stmt.chars().all(|c| matches!(c, '{' | '}' ) ) ;
                if !stmt.is_empty() && !bare_brace {
                    if let Some(frame) = stack.last_mut() {
                        let id = format!("{}.{}", frame.id, frame.children.len());
                        frame.children.push(leaf(&id, "qs_statement", stmt, lineno));
                    }
                }
            }
        }
    }

    // Drain unclosed frames
    while let Some(frame) = stack.pop() {
        let n = block_node(
            &frame.id,
            frame.node_type,
            &frame.label,
            frame.start_line,
            total_lines,
            frame.children,
        );
        // Don't use push_to here — stack is being drained, push to root
        root_children.push(n);
    }

    let root = SemanticNodeBuilder::new(
        "0",
        "compilation_unit",
        "compilation_unit",
        0,
        0,
        total_lines,
        0,
        String::new(),
    )
    .children(root_children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
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
}
