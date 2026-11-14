use crate::_base::text::line_endings::{LineEndingStyle, materialize_model_text, to_model_text_view};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextModel {
    pub line_ending_style: LineEndingStyle,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceAllResult {
    pub text: String,
    pub count: usize,
}

impl TextModel {
    pub fn new(raw: &str) -> Self {
        let view = to_model_text_view(raw);
        Self {
            text: view.text,
            line_ending_style: view.line_ending_style,
        }
    }

    pub fn count_occurrences(&self, needle: &str) -> usize {
        let mut count = 0;
        let mut pos = 0;
        while pos < self.text.len() {
            let Some(index) = self.text[pos..].find(needle).map(|index| pos + index) else {
                break;
            };
            count += 1;
            pos = index + needle.len();
        }
        count
    }

    pub fn replace_once(&self, needle: &str, replacement: &str) -> String {
        let Some(index) = self.text.find(needle) else {
            return self.text.clone();
        };
        let mut text = String::with_capacity(self.text.len() - needle.len() + replacement.len());
        text.push_str(&self.text[..index]);
        text.push_str(replacement);
        text.push_str(&self.text[index + needle.len()..]);
        text
    }

    pub fn replace_all(&self, needle: &str, replacement: &str) -> ReplaceAllResult {
        let parts = self.text.split(needle).collect::<Vec<_>>();
        ReplaceAllResult {
            text: parts.join(replacement),
            count: parts.len() - 1,
        }
    }

    pub fn materialize(&self, model_text: &str) -> String {
        materialize_model_text(model_text, self.line_ending_style)
    }
}
