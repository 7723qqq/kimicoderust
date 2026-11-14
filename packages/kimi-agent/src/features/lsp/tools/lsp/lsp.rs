use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::super::lsp::LspOperation;

pub const LSP_TOOL_SERVICE_ID: &str = "lspTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspInput {
    pub operation: LspOperation,
    pub file_path: String,
    pub line: u64,
    pub character: u64,
}

pub fn lsp_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["goToDefinition", "findReferences", "goToImplementation", "hover"],
                "description": "The semantic operation to run: goToDefinition (jump to the symbol definition), findReferences (all references to the symbol), goToImplementation (implementations of an interface/abstract member), hover (type and doc information at the position)."
            },
            "file_path": {
                "type": "string",
                "description": "Absolute path of the file to query."
            },
            "line": {
                "type": "integer",
                "minimum": 1,
                "description": "One-based line number of the position to query."
            },
            "character": {
                "type": "integer",
                "minimum": 1,
                "description": "One-based character offset (in UTF-16 code units) of the position to query."
            }
        },
        "required": ["operation", "file_path", "line", "character"],
        "additionalProperties": false
    })
}
