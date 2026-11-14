use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashSyntaxNode {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
    pub is_named: bool,
    pub children: Vec<BashSyntaxNode>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashParseOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nodes: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BashParseFailureReason {
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum BashParseResult {
    #[serde(rename = "true", rename_all = "camelCase")]
    Ok {
        has_error: bool,
        root: BashSyntaxNode,
    },
    #[serde(rename = "false", rename_all = "camelCase")]
    Err {
        reason: BashParseFailureReason,
    },
}

pub trait IBashParserService: Send + Sync {
    fn parse(&self, source: &str, options: Option<BashParseOptions>) -> BashParseResult;
}

pub static I_BASH_PARSER_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("bashParserService"));
