use std::collections::HashMap;
use std::ops::ControlFlow;
use std::time::Instant;

use tree_sitter::{Node, ParseOptions, ParseState, Parser, Point};

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;

use super::bash_parser::{
    BashParseFailureReason, BashParseOptions, BashParseResult, BashSyntaxNode, IBashParserService,
};

const DEFAULT_TIMEOUT_MS: f64 = 50.0;
const DEFAULT_MAX_NODES: f64 = 50_000.0;

#[derive(Default)]
pub struct BashParserService;

impl BashParserService {
    pub fn new() -> Self {
        Self
    }

    pub fn parse(&self, source: &str, options: Option<BashParseOptions>) -> BashParseResult {
        let options = options.unwrap_or_default();
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let max_nodes = options.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
        let started = Instant::now();
        if max_nodes < 1.0 || deadline_exceeded(started, timeout_ms) {
            return aborted();
        }

        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .is_err()
        {
            return BashParseResult::Ok {
                has_error: true,
                root: error_root(source),
            };
        }

        let bytes = source.as_bytes();
        let mut input = |offset: usize, _: Point| {
            if offset < bytes.len() {
                &bytes[offset..]
            } else {
                &[][..]
            }
        };
        let mut progress = |_: &ParseState| {
            if deadline_exceeded(started, timeout_ms) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let parse_options = ParseOptions::new().progress_callback(&mut progress);
        let Some(tree) = parser.parse_with_options(&mut input, None, Some(parse_options)) else {
            return aborted();
        };
        if deadline_exceeded(started, timeout_ms) {
            return aborted();
        }

        let root = tree.root_node();
        let has_error = root.has_error();
        match snapshot(root, source, max_nodes) {
            Some(root) => BashParseResult::Ok { has_error, root },
            None => aborted(),
        }
    }
}

impl IBashParserService for BashParserService {
    fn parse(&self, source: &str, options: Option<BashParseOptions>) -> BashParseResult {
        BashParserService::parse(self, source, options)
    }
}

fn aborted() -> BashParseResult {
    BashParseResult::Err {
        reason: BashParseFailureReason::Aborted,
    }
}

fn deadline_exceeded(started: Instant, timeout_ms: f64) -> bool {
    timeout_ms.is_finite() && started.elapsed().as_secs_f64() * 1000.0 >= timeout_ms
}

fn error_root(source: &str) -> BashSyntaxNode {
    let end_index = source.encode_utf16().count();
    BashSyntaxNode {
        type_: "program".to_string(),
        text: source.to_string(),
        start_index: 0,
        end_index,
        is_named: true,
        children: vec![BashSyntaxNode {
            type_: "ERROR".to_string(),
            text: source.to_string(),
            start_index: 0,
            end_index,
            is_named: true,
            children: Vec::new(),
        }],
    }
}

fn snapshot(root: Node<'_>, source: &str, max_nodes: f64) -> Option<BashSyntaxNode> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    let mut ids = HashMap::new();
    let mut dtos = Vec::new();
    while let Some(node) = stack.pop() {
        let index = order.len();
        order.push(node);
        ids.insert(node.id(), index);
        dtos.push(Some(BashSyntaxNode {
            type_: node.kind().to_string(),
            text: node_text(node, source).to_string(),
            start_index: utf16_index(source, node.start_byte()),
            end_index: utf16_index(source, node.end_byte()),
            is_named: node.is_named(),
            children: Vec::new(),
        }));
        if dtos.len() as f64 > max_nodes {
            return None;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    for index in (0..order.len()).rev() {
        let mut cursor = order[index].walk();
        let children = order[index].children(&mut cursor).collect::<Vec<_>>();
        for child in children {
            let child_index = ids[&child.id()];
            let child_dto = dtos[child_index]
                .take()
                .expect("each syntax node is attached exactly once");
            dtos[index]
                .as_mut()
                .expect("parent syntax node remains until its parent is attached")
                .children
                .push(child_dto);
        }
    }
    dtos.into_iter().next().flatten()
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source
        .get(node.start_byte()..node.end_byte())
        .expect("tree-sitter node ranges are UTF-8 boundaries")
}

fn utf16_index(source: &str, byte_index: usize) -> usize {
    source
        .get(..byte_index)
        .expect("tree-sitter node ranges are UTF-8 boundaries")
        .encode_utf16()
        .count()
}

pub fn register_bash_parser_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnDemand, "bashParser");
}
