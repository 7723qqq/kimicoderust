use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEndingStyle {
    Lf,
    CrLf,
    Mixed,
}

impl fmt::Display for LineEndingStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lf => write!(f, "lf"),
            Self::CrLf => write!(f, "crlf"),
            Self::Mixed => write!(f, "mixed"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct LineEndingFlags {
    pub has_crlf: bool,
    pub has_lf: bool,
    pub has_lone_cr: bool,
    pub has_bare_lf: bool,
}

impl LineEndingFlags {
    pub fn feed(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.has_lf = true;
                self.has_bare_lf = true;
            }
            b'\r' => self.has_lone_cr = true,
            _ => {}
        }
    }

    pub fn feed_crlf(&mut self) {
        self.has_crlf = true;
        self.has_lf = true;
    }

    pub fn style(&self) -> LineEndingStyle {
        if self.has_lone_cr || (self.has_crlf && self.has_bare_lf) {
            return LineEndingStyle::Mixed;
        }
        if self.has_crlf {
            return LineEndingStyle::CrLf;
        }
        LineEndingStyle::Lf
    }
}

pub fn detect_line_ending_style(data: &[u8]) -> LineEndingStyle {
    let mut flags = LineEndingFlags::default();
    let mut index = 0;
    while index < data.len() {
        if data[index] == b'\r' && index + 1 < data.len() && data[index + 1] == b'\n' {
            flags.feed_crlf();
            index += 2;
        } else {
            flags.feed(data[index]);
            index += 1;
        }
    }
    flags.style()
}

pub struct ModelTextView {
    pub text: String,
    pub line_ending_style: LineEndingStyle,
}

pub fn to_model_text_view(raw: &str) -> ModelTextView {
    let style = detect_line_ending_style(raw.as_bytes());
    let text = match style {
        LineEndingStyle::CrLf => raw.replace("\r\n", "\n"),
        _ => raw.to_string(),
    };
    ModelTextView {
        text,
        line_ending_style: style,
    }
}

pub fn materialize_model_text(text: &str, style: LineEndingStyle) -> String {
    match style {
        LineEndingStyle::CrLf => text.replace("\r\n", "\n").replace('\n', "\r\n"),
        _ => text.to_string(),
    }
}

pub fn make_carriage_returns_visible(text: &str) -> String {
    text.replace('\r', "\\r")
}

pub fn split_lines_keeping_terminator(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            lines.push(text[start..=index].to_string());
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}
