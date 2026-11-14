pub use crate::workflow::code_runtime::{
    CodeRunLimits, CodeRunOutcome, DEFAULT_TIMEOUT_MS, MAX_OUTPUT_CHARS, MAX_TIMEOUT_MS,
    MIN_TIMEOUT_MS, execute_code,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRunOptions {
    pub timeout_ms: Option<u64>,
    pub max_output_chars: Option<usize>,
}

impl Default for CodeRunOptions {
    fn default() -> Self {
        Self {
            timeout_ms: None,
            max_output_chars: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeExitRoute {
    pub memory_limit_exceeded: bool,
    pub output_limit_hit: bool,
    pub exit_code: i32,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeExitError {
    pub kind: &'static str,
    pub message: String,
}

pub fn classify_code_exit(route: &CodeExitRoute) -> CodeExitError {
    if route.output_limit_hit {
        return CodeExitError {
            kind: "output-limit",
            message: "program output exceeded the size limit".to_string(),
        };
    }
    if route.memory_limit_exceeded {
        return CodeExitError {
            kind: "memory-limit",
            message: "program exceeded the memory budget and was terminated".to_string(),
        };
    }
    let stderr = trim_js(&route.stderr);
    let stderr_note = if stderr.is_empty() {
        String::new()
    } else {
        let units: Vec<u16> = stderr.encode_utf16().collect();
        format!(
            ": {}",
            String::from_utf16_lossy(&units[units.len().saturating_sub(500)..])
        )
    };
    CodeExitError {
        kind: "worker-exit",
        message: format!(
            "code subprocess exited with code {} without reporting a result{stderr_note}",
            route.exit_code
        ),
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub async fn run_code_in_subprocess(code: &str, options: CodeRunOptions) -> CodeRunOutcome {
    let timeout_ms = options
        .timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    execute_code(
        code,
        CodeRunLimits {
            timeout_ms,
            max_output_chars: options.max_output_chars.unwrap_or(MAX_OUTPUT_CHARS),
            ..CodeRunLimits::default()
        },
    )
    .await
}
