use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspPosition {
    pub line: u64,
    pub character: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspLocation {
    pub uri: String,
    pub range: LspRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspLocationLink {
    pub target_uri: String,
    pub target_range: LspRange,
    pub target_selection_range: LspRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_selection_range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspMarkupContent {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspLanguageValue {
    pub language: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum LspMarkedString {
    Text(String),
    LanguageValue(LspLanguageValue),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum LspHoverContentPart {
    Markup(LspMarkupContent),
    Marked(LspMarkedString),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum LspHoverContents {
    Part(LspHoverContentPart),
    Parts(Vec<LspHoverContentPart>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspHover {
    pub contents: LspHoverContents,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspTextDocumentIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspTextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspTextDocumentPositionParams {
    pub text_document: LspTextDocumentIdentifier,
    pub position: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspWorkspaceFolder {
    pub uri: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspHoverCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_format: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspTextDocumentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover: Option<LspHoverCapabilities>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspWorkspaceCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_document: Option<LspTextDocumentCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<LspWorkspaceCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspInitializeParams {
    pub process_id: Option<u64>,
    pub root_uri: Option<String>,
    pub workspace_folders: Option<Vec<LspWorkspaceFolder>>,
    pub capabilities: LspClientCapabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover_provider: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_provider: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references_provider: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_provider: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspServerInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspInitializeResult {
    pub capabilities: LspServerCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_info: Option<LspServerInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDidOpenTextDocumentParams {
    pub text_document: LspTextDocumentItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDidCloseTextDocumentParams {
    pub text_document: LspTextDocumentIdentifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspWorkspaceConfigurationItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspWorkspaceConfigurationParams {
    pub items: Vec<LspWorkspaceConfigurationItem>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LspApplyWorkspaceEditParams {
    pub edit: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspApplyWorkspaceEditResult {
    pub applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspRequestMessage {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspResponseError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LspResponseMessage {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LspResponseError>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LspNotificationMessage {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum LspMessage {
    Request(LspRequestMessage),
    Response(LspResponseMessage),
    Notification(LspNotificationMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum LspDefinitionResponseValue {
    Location(LspLocation),
    Locations(Vec<LspLocation>),
    LocationLinks(Vec<LspLocationLink>),
}

pub type LspDefinitionResponse = Option<LspDefinitionResponseValue>;
pub type LspReferencesResponse = Option<Vec<LspLocation>>;
pub type LspHoverResponse = Option<LspHover>;
