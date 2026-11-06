//! Layout and style primitives for the component layer.
//!
//! Everything a component needs to turn its content into positioned cells,
//! and nothing that touches a terminal: width measurement, word wrapping, and
//! a style vocabulary the backends translate into their own escape sequences.
//! The reference is `packages/pi-tui/src/utils.ts` `wrapTextWithAnsi` /
//! `visibleWidth`, but the ANSI half stays out of here — the Rust side styles
//! through [`Style`] and only the terminal backend ever emits codes, so a
//! width bug cannot hide behind an escape sequence that a later stage forgot
//! to skip.
//!
//! ## Why measurement lives here
//!
//! Wrapping decides how many rows a message occupies, which decides what a
//! scrollback backend has to print and what an alt-screen backend has to
//! scroll. Two implementations of "how wide is this" would make one backend's
//! frame disagree with the other's, and the disagreement would only show up on
//! CJK text or an emoji — the cases nobody puts in a fixture.

use serde::{Deserialize, Serialize};

/// Display width of one character, in terminal cells.
///
/// Three cases decide everything downstream:
///
/// - **Wide** (East Asian Wide/Fullwidth, most emoji): 2 cells.
/// - **Zero-width** (combining marks, variation selectors, ZWJ, the BOM,
///   zero-width space/joiner): 0 cells — rendering them as 1 makes every line
///   after them look one column short.
/// - **Everything else**: 1 cell.
///
/// Deliberately table-free for the ranges that matter and conservative where a
/// table would be needed: a character this function cannot classify counts as
/// 1, which is what a terminal does for anything it lacks a table entry for.
/// Getting WIDE wrong is visible; getting ambiguous-width wrong is not, because
/// terminals disagree about it too.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    // Control characters occupy no cell. A tab is layout, not width: the
    // caller expands it before measuring (see `expand_tabs`).
    if cp == 0 || cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    if is_zero_width(cp) {
        return 0;
    }
    if is_wide(cp) { 2 } else { 1 }
}

fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        // Combining diacritical marks and the common marks that follow a base
        // character. A full table is hundreds of ranges; these are the ones
        // that appear in real transcripts (accented Latin, Indic vowel signs,
        // Devanagari/Thai marks, emoji skin-tone modifiers).
        0x0300..=0x036f
        | 0x0483..=0x0489
        | 0x0591..=0x05bd
        | 0x0610..=0x061a
        | 0x064b..=0x065f
        | 0x0900..=0x0903
        | 0x093a..=0x094f
        | 0x0e31 | 0x0e34..=0x0e3a | 0x0e47..=0x0e4e
        // Formatting / invisible.
        | 0x200b..=0x200f
        | 0x202a..=0x202e
        | 0x2060..=0x2064
        | 0x2066..=0x206f
        | 0xfeff
        // Variation selectors and the ZWJ that joins emoji sequences.
        | 0xfe00..=0xfe0f
        | 0x20d0..=0x20f0
        | 0x1f3fb..=0x1f3ff
    )
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        // East Asian Wide / Fullwidth, plus the emoji blocks terminals render
        // two cells wide. Ordered by range start.
        0x1100..=0x115f        // Hangul Jamo initial
        | 0x2e80..=0x303e      // CJK radicals, Kangxi, CJK symbols
        | 0x3041..=0x33ff      // Hiragana, Katakana, CJK compat
        | 0x3400..=0x4dbf      // CJK ext A
        | 0x4e00..=0x9fff      // CJK unified
        | 0xa000..=0xa4cf      // Yi
        | 0xa960..=0xa97f      // Hangul Jamo extended A
        | 0xac00..=0xd7a3      // Hangul syllables
        | 0xf900..=0xfaff      // CJK compat ideographs
        | 0xfe10..=0xfe19      // Vertical forms
        | 0xfe30..=0xfe6f      // CJK compat forms
        | 0xff00..=0xff60      // Fullwidth forms
        | 0xffe0..=0xffe6      // Fullwidth signs
        | 0x1f300..=0x1f64f    // Misc symbols and pictographs, emoticons
        | 0x1f900..=0x1f9ff    // Supplemental symbols and pictographs
        | 0x1f680..=0x1f6ff    // Transport and map symbols
        | 0x1fa70..=0x1faff    // Symbols and pictographs extended-A
        | 0x20000..=0x3fffd    // CJK ext B and beyond
    )
}

/// Display width of a string, in terminal cells.
pub fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Replace tabs with spaces at the next multiple of `tab_width`.
///
/// Terminals disagree about tab stops (and a tab inside a styled span breaks
/// the width arithmetic that a wrapped line depends on), so the layout layer
/// expands them before anything measures.
pub fn expand_tabs(text: &str, tab_width: usize) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let tab_width = tab_width.max(1);
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    for c in text.chars() {
        match c {
            '\t' => {
                let advance = tab_width - (column % tab_width);
                for _ in 0..advance {
                    out.push(' ');
                }
                column += advance;
            }
            '\n' => {
                out.push('\n');
                column = 0;
            }
            other => {
                out.push(other);
                column += char_width(other);
            }
        }
    }
    out
}

/// One laid-out row: the text to draw and the column it started at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub text: String,
    /// The style of the source span this row came from.
    pub style: Style,
}

impl Row {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    pub fn width(&self) -> usize {
        width(&self.text)
    }
}

/// Wrap `text` to `max_width` columns.
///
/// Semantics, matching the TS reference (`wrapTextWithAnsi`):
///
/// - an empty string yields one empty row, not zero — a message with no text
///   still occupies a line;
/// - hard newlines split rows and are never joined;
/// - a run of whitespace is a wrap point and is dropped at the break (a line
///   never starts with the whitespace that caused the break);
/// - a token wider than the whole line is broken across rows rather than
///   overflowing (a 60-column URL in a 40-column terminal has to go somewhere);
/// - trailing whitespace is trimmed from each row.
///
/// `max_width == 0` is treated as 1: a zero-width line cannot make progress,
/// and looping forever on it is the worse failure.
pub fn wrap(text: &str, max_width: usize, style: Style) -> Vec<Row> {
    let max_width = max_width.max(1);
    if text.is_empty() {
        return vec![Row::new("", style)];
    }
    let mut rows: Vec<Row> = Vec::new();
    for source_line in text.split('\n') {
        wrap_line(source_line, max_width, style, &mut rows);
    }
    if rows.is_empty() {
        rows.push(Row::new("", style));
    }
    rows
}

fn wrap_line(line: &str, max_width: usize, style: Style, out: &mut Vec<Row>) {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        out.push(Row::new("", style));
        return;
    }
    if width(line) <= max_width {
        out.push(Row::new(line.trim_end(), style));
        return;
    }

    let mut current = String::new();
    let mut current_width = 0usize;

    for token in tokenize(line) {
        let token_width = width(&token);
        let is_space = token.chars().all(char::is_whitespace);

        // A token that does not fit even on an empty line: break it.
        if token_width > max_width && !is_space {
            if !current.is_empty() {
                out.push(Row::new(current.trim_end(), style));
                current.clear();
            }
            let mut chunk = String::new();
            let mut chunk_width = 0usize;
            for c in token.chars() {
                let w = char_width(c);
                if chunk_width + w > max_width && !chunk.is_empty() {
                    out.push(Row::new(std::mem::take(&mut chunk), style));
                    chunk_width = 0;
                }
                chunk.push(c);
                chunk_width += w;
            }
            current = chunk;
            current_width = chunk_width;
            continue;
        }

        if current_width + token_width > max_width && current_width > 0 {
            out.push(Row::new(current.trim_end(), style));
            current.clear();
            current_width = 0;
            // The whitespace that caused the break does not start the next row.
            if is_space {
                continue;
            }
        }
        current.push_str(&token);
        current_width += token_width;
    }

    if !current.is_empty() {
        out.push(Row::new(current.trim_end(), style));
    }
}

/// Split into wrap units: runs of non-whitespace, and runs of whitespace.
///
/// CJK has no spaces, so a run of wide characters would be one token and get
/// hard-broken mid-word. Splitting between wide characters instead gives the
/// line-breaking behaviour those scripts expect, and costs nothing for Latin
/// text (which has no wide characters inside a word).
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut buf_is_space: Option<bool> = None;

    for c in line.chars() {
        let is_space = c.is_whitespace();
        let wide = char_width(c) == 2;
        let starts_new = match buf_is_space {
            None => true,
            Some(prev_space) => prev_space != is_space || wide,
        };
        if starts_new && !buf.is_empty() {
            tokens.push(std::mem::take(&mut buf));
        }
        buf_is_space = Some(is_space);
        buf.push(c);
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    tokens
}

/// A cell colour, matching the 16 ANSI names plus 24-bit RGB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Color {
    #[default]
    Default,
    /// One of the 16 ANSI colours, 0-15.
    Ansi(u8),
    Rgb {
        r: u8,
        g: u8,
        b: u8,
    },
}

/// Text attributes. A set, because bold+italic+underline is one combination a
/// backend has to emit as several codes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Attributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reversed: bool,
}

impl Attributes {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// Style of a laid-out row or span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attributes: Attributes,
}

impl Style {
    pub const DEFAULT: Style = Style {
        fg: Color::Default,
        bg: Color::Default,
        attributes: Attributes {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reversed: false,
        },
    };

    pub fn fg(color: Color) -> Self {
        Self {
            fg: color,
            ..Self::DEFAULT
        }
    }

    pub fn bold(mut self) -> Self {
        self.attributes.bold = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.attributes.dim = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.attributes.italic = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.attributes.underline = true;
        self
    }

    /// Whether drawing this style changes anything at all. A backend uses it
    /// to skip emitting SGR codes for plain text.
    pub fn is_plain(self) -> bool {
        self == Self::DEFAULT
    }
}

/// A laid-out block: rows plus the width they were laid out for.
///
/// Keeping the width with the rows is what lets a caller tell "these rows are
/// already correct for the current terminal" from "they were wrapped for a
/// width the terminal no longer has" — the resize case, where re-wrapping from
/// the original text is required and re-using the rows silently truncates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    rows: Vec<Row>,
    width: usize,
}

impl Block {
    pub fn new(rows: Vec<Row>, width: usize) -> Self {
        Self { rows, width }
    }

    /// Lay out `text` at `width` in one call.
    pub fn wrap(text: &str, width: usize, style: Style) -> Self {
        Self {
            rows: wrap(text, width, style),
            width: width.max(1),
        }
    }

    pub fn empty(width: usize) -> Self {
        Self {
            rows: Vec::new(),
            width: width.max(1),
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether this block was laid out for a different width and has to be
    /// re-wrapped from its source text.
    pub fn needs_relayout(&self, width: usize) -> bool {
        self.width != width.max(1)
    }

    /// The widest row, which is what a caller aligning or padding boxes needs.
    pub fn content_width(&self) -> usize {
        self.rows.iter().map(Row::width).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_and_zero_width_characters_are_measured_separately() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("中文"), 4, "CJK is two cells per character");
        assert_eq!(width("café"), 4, "precomposed é is one cell");
        assert_eq!(
            width("cafe\u{0301}"),
            4,
            "combining acute occupies no cell of its own"
        );
        assert_eq!(width("\u{200b}"), 0, "zero-width space");
        assert_eq!(width("\u{feff}"), 0, "BOM");
        assert_eq!(width("🙂"), 2, "emoji are wide");
        assert_eq!(width("👍\u{1f3fd}"), 2, "a skin-tone modifier adds nothing");
        assert_eq!(width("\t"), 0, "a tab is layout, not width");
        assert_eq!(
            width("\u{1b}[31m"),
            4,
            "escape bytes measure as written (ESC itself is zero-width)"
        );
    }

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(expand_tabs("a\tb", 4), "a   b");
        assert_eq!(expand_tabs("abcd\te", 4), "abcd    e");
        assert_eq!(
            expand_tabs("a\nb\tc", 4),
            "a\nb   c",
            "newline resets the column"
        );
        assert_eq!(expand_tabs("no tabs", 4), "no tabs");
    }

    #[test]
    fn empty_text_is_one_empty_row_not_zero() {
        let rows = wrap("", 10, Style::DEFAULT);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "");
    }

    #[test]
    fn short_text_is_one_row() {
        let rows = wrap("hello", 20, Style::DEFAULT);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello");
    }

    #[test]
    fn wraps_on_word_boundaries_and_trims_the_break_space() {
        // "alpha beta gamma" at width 10: "alpha beta" is exactly 10, so gamma
        // goes to the next row.
        let rows = wrap("alpha beta gamma", 10, Style::DEFAULT);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["alpha beta", "gamma"]
        );

        // The whitespace that caused the break never starts a row.
        let rows = wrap("alpha      beta", 8, Style::DEFAULT);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn an_over_long_token_is_broken_rather_than_overflowing() {
        let url = "https://example.test/a/very/long/path/that/cannot/wrap";
        let rows = wrap(url, 12, Style::DEFAULT);
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(row.width() <= 12, "row overflows: {:?}", row.text);
        }
        // Nothing was lost.
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<String>(),
            url
        );
    }

    #[test]
    fn hard_newlines_split_rows_and_keep_empty_lines() {
        let rows = wrap("a\n\nb", 10, Style::DEFAULT);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "", "b"]
        );
    }

    #[test]
    fn crlf_does_not_leave_a_carriage_return_on_the_row() {
        let rows = wrap("a\r\nb", 10, Style::DEFAULT);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn cjk_wraps_between_characters() {
        let rows = wrap("中文字符测试", 4, Style::DEFAULT);
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(row.width() <= 4, "{:?} is {} cells", row.text, row.width());
        }
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<String>(),
            "中文字符测试"
        );
    }

    #[test]
    fn a_zero_width_limit_is_treated_as_one() {
        let rows = wrap("ab", 0, Style::DEFAULT);
        assert!(rows.iter().all(|r| r.width() <= 1));
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<String>(),
            "ab"
        );
    }

    #[test]
    fn style_reaches_every_row() {
        let bold = Style::DEFAULT.bold();
        let rows = wrap("alpha beta gamma", 10, bold);
        assert!(rows.iter().all(|r| r.style == bold));
    }

    #[test]
    fn block_reports_when_it_needs_relayout() {
        let block = Block::wrap("hello", 20, Style::DEFAULT);
        assert!(!block.needs_relayout(20));
        assert!(
            block.needs_relayout(10),
            "narrower terminal needs a re-wrap"
        );
        assert_eq!(block.width(), 20);
        assert_eq!(block.height(), 1);
        assert_eq!(block.content_width(), 5);
    }

    #[test]
    fn plain_style_is_distinguishable_from_styled() {
        assert!(Style::DEFAULT.is_plain());
        assert!(!Style::DEFAULT.bold().is_plain());
        assert!(!Style::fg(Color::Ansi(1)).is_plain());
        assert!(Style::DEFAULT.attributes.is_empty());
    }
}
