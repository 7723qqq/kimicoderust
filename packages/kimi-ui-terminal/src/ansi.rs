//! SGR encoding: a [`Style`] to the escape sequence that draws it.
//!
//! One place emits colour, so the two render strategies cannot disagree about
//! what `Style::bold().fg(Ansi(4))` looks like. Nothing here reads or writes a
//! terminal — it appends to a `String` the caller is building.

use kimi_ui_core::{Attributes, Color, Style};

/// Reset all attributes. Emitted after every styled row, including one that
/// changed nothing, so a row can never inherit the previous row's colour.
pub const RESET: &str = "\x1b[0m";

/// The SGR parameters that draw `style`, without the `ESC [` wrapper.
///
/// Empty when the style is plain: a terminal that receives `ESC [ m` for
/// uncoloured text pays a write and a parser round trip for nothing.
pub fn sgr_params(style: Style) -> String {
    if style.is_plain() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    push_attributes(style.attributes, &mut parts);
    push_color(style.fg, false, &mut parts);
    push_color(style.bg, true, &mut parts);
    parts.join(";")
}

/// `ESC [ … m` for `style`, or an empty string when it is plain.
pub fn open(style: Style) -> String {
    let params = sgr_params(style);
    if params.is_empty() {
        String::new()
    } else {
        format!("\x1b[{params}m")
    }
}

/// Write `style`'s opening sequence, the text, and a reset — unless the style
/// is plain, in which case only the text.
pub fn push_styled(out: &mut String, text: &str, style: Style) {
    if style.is_plain() {
        out.push_str(text);
        return;
    }
    out.push_str(&open(style));
    out.push_str(text);
    out.push_str(RESET);
}

fn push_attributes(attributes: Attributes, parts: &mut Vec<String>) {
    // Order is fixed so a golden test can compare sequences: SGR parameters
    // are a set, and two orders should not produce two different strings.
    for (on, code) in [
        (attributes.bold, 1),
        (attributes.dim, 2),
        (attributes.italic, 3),
        (attributes.underline, 4),
        (attributes.reversed, 7),
    ] {
        if on {
            parts.push(code.to_string());
        }
    }
}

fn push_color(color: Color, background: bool, parts: &mut Vec<String>) {
    match color {
        Color::Default => {}
        // 0-7 are fg 30-37 / bg 40-47; 8-15 are the bright pair, 90-97 / 100-107.
        Color::Ansi(index) => {
            let base = match (background, index < 8) {
                (false, true) => 30,
                (true, true) => 40,
                (false, false) => 90,
                (true, false) => 100,
            };
            parts.push((base + index % 8).to_string());
        }
        Color::Rgb { r, g, b } => {
            parts.push(format!(
                "{};2;{r};{g};{b}",
                if background { 48 } else { 38 }
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_style_emits_nothing() {
        assert_eq!(sgr_params(Style::DEFAULT), "");
        assert_eq!(open(Style::DEFAULT), "");

        let mut out = String::new();
        push_styled(&mut out, "hello", Style::DEFAULT);
        assert_eq!(out, "hello");
    }

    #[test]
    fn attributes_come_out_in_a_fixed_order() {
        let style = Style::DEFAULT.bold().dim().italic().underline();
        assert_eq!(sgr_params(style), "1;2;3;4");
    }

    #[test]
    fn the_sixteen_ansi_colours_map_to_the_standard_pair() {
        assert_eq!(open(Style::fg(Color::Ansi(1))), "\x1b[31m");
        assert_eq!(open(Style::fg(Color::Ansi(7))), "\x1b[37m");
        assert_eq!(open(Style::fg(Color::Ansi(8))), "\x1b[90m");
        assert_eq!(open(Style::fg(Color::Ansi(15))), "\x1b[97m");
    }

    #[test]
    fn a_background_colour_uses_the_background_range() {
        let style = Style {
            bg: Color::Ansi(2),
            ..Style::DEFAULT
        };
        assert_eq!(open(style), "\x1b[42m");
        let bright = Style {
            bg: Color::Ansi(9),
            ..Style::DEFAULT
        };
        assert_eq!(open(bright), "\x1b[101m");
    }

    #[test]
    fn rgb_uses_the_extended_form() {
        let style = Style::fg(Color::Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(open(style), "\x1b[38;2;1;2;3m");
        let background = Style {
            bg: Color::Rgb { r: 4, g: 5, b: 6 },
            ..Style::DEFAULT
        };
        assert_eq!(open(background), "\x1b[48;2;4;5;6m");
    }

    #[test]
    fn styled_text_is_reset_after_the_run() {
        let mut out = String::new();
        push_styled(&mut out, "x", Style::DEFAULT.bold());
        assert_eq!(out, "\x1b[1mx\x1b[0m");
    }
}
