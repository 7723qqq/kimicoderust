//! The terminal backend: paints the shared component tree as ANSI bytes.
//!
//! Two render strategies, mirroring the TS side's `TuiMainScreen` /
//! `TuiAltScreen` split (design `reports/design/RUST-UI-DECOUPLING.md` §1.3):
//!
//! - **Scrollback** appends only the lines the terminal has not printed yet and
//!   lets the terminal own scrolling. Nothing above the divergence point can be
//!   corrected, which is why a change up there becomes a full repaint request.
//! - **Alt screen** takes the viewport and repaints it in place.
//!
//! What this type deliberately does not do: enter raw mode, decode keys, or
//! capture the mouse. Those need the terminal handle an application owns, and
//! `kimi-ui-core`'s [`InputEvent`] is already the neutral shape the app decodes
//! into — a second decoder down here would have to guess at the same platform
//! quirks twice. The backend writes bytes; the app decides what terminal it is
//! writing to.
//!
//! `render` cannot report an I/O error: [`RenderBackend::render`] returns the
//! frame it emitted, not a `Result`. A failed write is therefore kept on the
//! backend ([`TerminalBackend::take_error`]) rather than dropped — a terminal
//! that went away mid-frame is a fact the host has to see.

use std::io::{self, Write};

use kimi_ui_core::{
    BackendState, CapabilitySet, ComponentNode, Frame, InputEvent, NodeId, RenderBackend,
    StopOptions, Style,
};

use crate::ansi;

/// Where the backend paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Append to the terminal's own scrollback; the terminal scrolls.
    Scrollback,
    /// Own the viewport in the alternative screen buffer.
    AltScreen,
}

/// Enter the alternative screen buffer and home the cursor.
const ALT_SCREEN_ENTER: &str = "\x1b[?1049h\x1b[H";
/// Leave the alternative screen buffer, restoring what was on screen before.
const ALT_SCREEN_LEAVE: &str = "\x1b[?1049l";
/// Cursor home.
const HOME: &str = "\x1b[H";
/// Erase from the cursor to the end of the line.
const CLEAR_LINE: &str = "\x1b[K";
/// Erase from the cursor to the end of the screen.
const CLEAR_BELOW: &str = "\x1b[J";

/// A [`RenderBackend`] that paints into any [`Write`].
///
/// Generic over the sink so a test can read the bytes back and an application
/// can hand in stdout: the rendering decisions and the terminal device stay
/// separate, and neither needs a pty to be exercised.
pub struct TerminalBackend<W: Write> {
    state: BackendState,
    mode: Mode,
    out: W,
    /// Alt screen only: whether bytes of a frame are currently on screen, which
    /// `stop` needs before deciding to repaint.
    painted: bool,
    /// How many lines the terminal currently holds. Scrollback keeps the last
    /// of them **open** (no newline written after it) so a streamed append can
    /// rewrite that line in place instead of printing the transcript again.
    painted_lines: usize,
    /// Whether to emit SGR at all. A pipe, a `NO_COLOR` terminal or a dumb
    /// terminal gets the same text without the escape sequences.
    color: bool,
    /// The first write failure since the last [`Self::take_error`].
    last_error: Option<io::Error>,
}

impl<W: Write> TerminalBackend<W> {
    pub fn new(mode: Mode, out: W) -> Self {
        Self {
            state: BackendState::default(),
            mode,
            out,
            painted: false,
            painted_lines: 0,
            color: true,
            last_error: None,
        }
    }

    /// Turn SGR emission off (or back on). Text is unchanged; only the escape
    /// sequences disappear.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The sink, for a host that shares it with something else.
    pub fn writer(&self) -> &W {
        &self.out
    }

    /// The first write error since the last call, if any. Taken rather than
    /// read so a host can report each failure once.
    pub fn take_error(&mut self) -> Option<io::Error> {
        self.last_error.take()
    }

    /// Every laid-out line with the style it draws in, in paint order.
    ///
    /// The layout happens here, once, and serves both the diff (which needs the
    /// text) and the paint (which needs the style).
    fn styled_lines(&mut self) -> Vec<(String, Style)> {
        let width = self.state.paint_width();
        let mut lines = Vec::new();
        for run in self.state.styled_rows(width) {
            for line in run.lines {
                lines.push((line, run.style));
            }
        }
        lines
    }

    fn write_styled_line(&mut self, line: &str, style: Style) {
        if self.last_error.is_some() {
            return;
        }
        let mut buf = String::new();
        if self.color {
            ansi::push_styled(&mut buf, line, style);
        } else {
            buf.push_str(line);
        }
        if let Err(error) = self.out.write_all(buf.as_bytes()) {
            self.last_error = Some(error);
        }
    }

    fn write_raw(&mut self, text: &str) {
        if self.last_error.is_some() {
            return;
        }
        if let Err(error) = self.out.write_all(text.as_bytes()) {
            self.last_error = Some(error);
        }
    }

    fn flush(&mut self) {
        if self.last_error.is_some() {
            return;
        }
        if let Err(error) = self.out.flush() {
            self.last_error = Some(error);
        }
    }

    /// Append to the scrollback, keeping the last line **open**.
    ///
    /// The open line is what makes streaming work here: a token append changes
    /// the last line, and a scrollback terminal cannot retract a line it has
    /// terminated. So the previous paint left the cursor on that line, and this
    /// pass rewrites it in place (`\r`, the new text, `\x1b[K`) instead of
    /// reprinting the transcript below itself.
    ///
    /// Everything above the open line is closed and cannot be corrected; a
    /// change up there — or a frame that lost lines — falls back to a full
    /// repaint, which is the caller's cue to accept duplicated output or pick
    /// the alt-screen mode.
    fn paint_scrollback(&mut self, styled: &[(String, Style)]) -> Vec<String> {
        let len = styled.len();
        let prev_len = self.painted_lines;
        let common = styled
            .iter()
            .zip(self.state.last_frame.lines())
            .take_while(|((line, _), previous)| line == *previous)
            .count();
        let unfixable =
            self.state.force || (prev_len > 0 && (common + 1 < prev_len || len < prev_len));
        let mut emitted = Vec::new();

        if unfixable || prev_len == 0 {
            for (index, (line, style)) in styled.iter().enumerate() {
                self.write_styled_line(line, *style);
                if index + 1 < len {
                    self.write_raw("\r\n");
                }
                emitted.push(line.clone());
            }
            if len > 0 {
                self.write_raw(CLEAR_LINE);
            }
        } else {
            let open = prev_len - 1;
            if common <= open {
                // The open line changed under the cursor: rewrite it.
                self.write_raw("\r");
                self.write_styled_line(&styled[open].0, styled[open].1);
                self.write_raw(CLEAR_LINE);
                emitted.push(styled[open].0.clone());
            }
            if len > prev_len {
                // Finalise the open line and print what follows it.
                self.write_raw("\r\n");
                for (index, (line, style)) in styled.iter().enumerate().skip(prev_len) {
                    self.write_styled_line(line, *style);
                    if index + 1 < len {
                        self.write_raw("\r\n");
                    }
                    emitted.push(line.clone());
                }
                self.write_raw(CLEAR_LINE);
            }
        }

        self.painted_lines = len;
        self.flush();
        emitted
    }

    /// Repaint the viewport in place: the alt screen owns the whole screen, so
    /// there is nothing to preserve and no open line to keep.
    fn paint_alt_screen(&mut self, styled: &[(String, Style)]) -> Vec<String> {
        let changed = styled
            .iter()
            .map(|(line, _)| line)
            .ne(self.state.last_frame.lines().iter());
        if !changed && !self.state.force {
            return Vec::new();
        }
        self.write_raw(HOME);
        let last = styled.len().saturating_sub(1);
        let mut emitted = Vec::with_capacity(styled.len());
        for (index, (line, style)) in styled.iter().enumerate() {
            self.write_styled_line(line, *style);
            self.write_raw(CLEAR_LINE);
            if index != last {
                self.write_raw("\r\n");
            }
            emitted.push(line.clone());
        }
        self.write_raw(CLEAR_BELOW);
        self.painted = true;
        self.painted_lines = styled.len();
        self.flush();
        emitted
    }
}

impl<W: Write> RenderBackend for TerminalBackend<W> {
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> Option<NodeId> {
        self.state.mount(node, parent)
    }

    fn unmount(&mut self, id: NodeId) {
        self.state.unmount(id);
    }

    fn request_render(&mut self, force: bool) {
        self.state.request_render(force);
    }

    fn frame(&self) -> Frame {
        self.state.last_frame.clone()
    }

    fn render(&mut self) -> Frame {
        if !self.state.pending {
            return Frame::empty();
        }
        let styled = self.styled_lines();
        let emitted = match self.mode {
            Mode::Scrollback => self.paint_scrollback(&styled),
            Mode::AltScreen => self.paint_alt_screen(&styled),
        };
        self.state.last_frame = Frame::new(styled.iter().map(|(line, _)| line.clone()));
        self.state.pending = false;
        self.state.force = false;
        Frame::new(emitted)
    }

    fn start(&mut self) -> io::Result<()> {
        if self.mode == Mode::AltScreen {
            self.write_raw(ALT_SCREEN_ENTER);
            self.flush();
        }
        self.state.started = true;
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        Ok(())
    }

    /// Leave the mode's screen. `keep_frame` repaints the last frame onto the
    /// main buffer after leaving the alternative one — otherwise leaving the
    /// alt screen restores the shell, and the transcript is gone.
    fn stop(&mut self, opts: StopOptions) -> io::Result<()> {
        if self.mode == Mode::AltScreen {
            self.write_raw(ALT_SCREEN_LEAVE);
            if opts.keep_frame {
                for (line, style) in self.styled_lines_clone() {
                    self.write_styled_line(&line, style);
                    self.write_raw("\r\n");
                }
            }
            self.painted = false;
        } else if self.painted_lines > 0 {
            // The last line was left open for streaming; terminate it so the
            // shell prompt does not land on the transcript's last row.
            self.write_raw("\r\n");
        }
        self.painted_lines = 0;
        self.flush();
        self.state.started = false;
        self.state.input.clear();
        if let Some(error) = self.take_error() {
            return Err(error);
        }
        Ok(())
    }

    fn dispatch_input(&mut self, ev: InputEvent) {
        self.state.dispatch_input(ev);
    }

    fn set_focus(&mut self, id: Option<NodeId>) {
        self.state.set_focus(id);
    }

    fn focused(&self) -> Option<NodeId> {
        self.state.focus
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet {
            // Only the passthrough mode leaves scrolling to the terminal. The
            // alt screen owns the viewport, but nothing here implements
            // mouse-driven scrolling, selection or search yet, and reporting
            // them would make a component offer keys that do nothing.
            scrollback_passthrough: self.mode == Mode::Scrollback,
            osc8_links: true,
            ..CapabilitySet::default()
        }
    }
}

impl<W: Write> TerminalBackend<W> {
    /// The frame's lines with styles, for `stop`'s keep-the-frame repaint.
    fn styled_lines_clone(&mut self) -> Vec<(String, Style)> {
        self.styled_lines()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(mode: Mode) -> TerminalBackend<Vec<u8>> {
        TerminalBackend::new(mode, Vec::new())
    }

    fn painted(backend: &TerminalBackend<Vec<u8>>) -> String {
        String::from_utf8_lossy(backend.writer()).into_owned()
    }

    fn mount_text<W: Write>(backend: &mut TerminalBackend<W>, text: &str) -> NodeId {
        backend.mount(ComponentNode::text(text), None).unwrap()
    }

    #[test]
    fn scrollback_prints_only_the_lines_it_has_not_printed_yet() {
        let mut backend = backend(Mode::Scrollback);
        backend.dispatch_input(InputEvent::Resize {
            columns: 20,
            rows: 10,
        });
        mount_text(&mut backend, "first");
        backend.request_render(false);
        let first = backend.render();
        assert_eq!(first.lines(), ["first"]);
        assert!(painted(&backend).contains("first"));

        mount_text(&mut backend, "second");
        backend.request_render(false);
        let second = backend.render();
        assert_eq!(
            second.lines(),
            ["second"],
            "an appended line is the only new row"
        );
        let out = painted(&backend);
        assert_eq!(out.matches("first").count(), 1, "first was not reprinted");
        assert_eq!(out.matches("second").count(), 1);
    }

    #[test]
    fn a_forced_pass_reprints_the_whole_frame() {
        let mut backend = backend(Mode::Scrollback);
        mount_text(&mut backend, "line");
        backend.request_render(false);
        backend.render();
        backend.request_render(true);
        let emitted = backend.render();
        assert_eq!(emitted.lines(), ["line"]);
        assert_eq!(painted(&backend).matches("line").count(), 2);
    }

    #[test]
    fn a_render_with_nothing_pending_emits_nothing() {
        let mut backend = backend(Mode::Scrollback);
        mount_text(&mut backend, "line");
        assert!(backend.render().is_empty());
        assert_eq!(painted(&backend), "");
    }

    #[test]
    fn alt_screen_homes_the_cursor_and_clears_the_rest_of_the_screen() {
        let mut backend = backend(Mode::AltScreen);
        mount_text(&mut backend, "a");
        backend.request_render(false);
        backend.render();
        let out = painted(&backend);
        assert!(
            out.starts_with(HOME),
            "a repaint starts at the home position"
        );
        assert!(out.contains("a"));
        assert!(out.contains(CLEAR_LINE));
        assert!(out.ends_with(CLEAR_BELOW));
    }

    #[test]
    fn alt_screen_repaints_the_whole_viewport_on_the_second_pass() {
        let mut backend = backend(Mode::AltScreen);
        mount_text(&mut backend, "a");
        backend.request_render(false);
        backend.render();
        mount_text(&mut backend, "b");
        backend.request_render(false);
        backend.render();
        let out = painted(&backend);
        assert_eq!(out.matches('a').count(), 2, "the first row is repainted");
        assert_eq!(out.matches('b').count(), 1);
    }

    #[test]
    fn plain_text_carries_no_escape_sequences_when_colour_is_off() {
        let mut backend = backend(Mode::Scrollback).with_color(false);
        backend.mount(ComponentNode::styled("bold", Style::DEFAULT.bold()), None);
        backend.request_render(false);
        backend.render();
        // The line is left open for streaming, and cleared to its end.
        assert_eq!(painted(&backend), format!("bold{CLEAR_LINE}"));
    }

    #[test]
    fn a_styled_row_is_wrapped_in_its_sgr_sequence() {
        let mut backend = backend(Mode::Scrollback);
        backend.mount(ComponentNode::styled("x", Style::DEFAULT.bold()), None);
        backend.request_render(false);
        backend.render();
        assert_eq!(painted(&backend), format!("\x1b[1mx\x1b[0m{CLEAR_LINE}"));
    }

    #[test]
    fn a_streamed_append_rewrites_the_open_line_instead_of_reprinting() {
        let mut backend = backend(Mode::Scrollback);
        let first = mount_text(&mut backend, "one");
        backend.request_render(false);
        backend.render();

        // A streamed delta grows the last entry; the app re-mounts it.
        backend.unmount(first);
        mount_text(&mut backend, "one two");
        backend.request_render(false);
        let emitted = backend.render();
        assert_eq!(emitted.lines(), ["one two"]);
        let out = painted(&backend);
        assert!(out.contains("\rone two"), "rewritten in place: {out:?}");
        assert_eq!(
            out.matches("\r\n").count(),
            0,
            "nothing was terminated or reprinted: {out:?}"
        );
    }

    #[test]
    fn a_new_line_finalises_the_open_one() {
        let mut backend = backend(Mode::Scrollback);
        mount_text(&mut backend, "first");
        backend.request_render(false);
        backend.render();
        mount_text(&mut backend, "second");
        backend.request_render(false);
        let emitted = backend.render();
        assert_eq!(emitted.lines(), ["second"]);
        let out = painted(&backend);
        assert!(out.contains("\r\nsecond"), "{out:?}");
        assert_eq!(
            out.matches("first").count(),
            1,
            "the closed line is not reprinted: {out:?}"
        );
    }

    #[test]
    fn stopping_finalises_the_open_line() {
        let mut backend = backend(Mode::Scrollback);
        backend.start().unwrap();
        mount_text(&mut backend, "tail");
        backend.request_render(false);
        backend.render();
        backend.stop(StopOptions::default()).unwrap();
        assert!(painted(&backend).ends_with("\r\n"));
    }

    #[test]
    fn a_wrapped_row_styles_every_piece_it_produces() {
        let mut backend = backend(Mode::Scrollback);
        backend.dispatch_input(InputEvent::Resize {
            columns: 4,
            rows: 10,
        });
        backend.mount(
            ComponentNode::styled("alpha beta", Style::DEFAULT.underline()),
            None,
        );
        backend.request_render(false);
        let emitted = backend.render();
        assert!(emitted.len() > 1, "the text wraps at four columns");
        let out = painted(&backend);
        assert_eq!(
            out.matches("\x1b[4m").count(),
            emitted.len(),
            "one SGR open per painted row"
        );
        assert_eq!(out.matches(ansi::RESET).count(), emitted.len());
    }

    #[test]
    fn starting_the_alt_screen_enters_the_alternative_buffer() {
        let mut backend = backend(Mode::AltScreen);
        backend.start().unwrap();
        assert_eq!(painted(&backend), ALT_SCREEN_ENTER);
    }

    #[test]
    fn stopping_the_alt_screen_leaves_the_alternative_buffer() {
        let mut backend = backend(Mode::AltScreen);
        backend.start().unwrap();
        mount_text(&mut backend, "a");
        backend.request_render(false);
        backend.render();
        backend.stop(StopOptions::default()).unwrap();
        let out = painted(&backend);
        assert!(out.contains(ALT_SCREEN_LEAVE));
        assert_eq!(out.matches('a').count(), 1, "the frame is not repainted");
    }

    #[test]
    fn stopping_with_keep_frame_repaints_after_leaving() {
        let mut backend = backend(Mode::AltScreen);
        backend.start().unwrap();
        mount_text(&mut backend, "kept");
        backend.request_render(false);
        backend.render();
        backend.stop(StopOptions { keep_frame: true }).unwrap();
        let out = painted(&backend);
        let after_leave = out
            .split_once(ALT_SCREEN_LEAVE)
            .expect("the alt screen is left first")
            .1;
        assert!(
            after_leave.contains("kept"),
            "the transcript survives leaving the alt screen"
        );
    }

    #[test]
    fn the_two_modes_report_different_capabilities() {
        let scrollback = backend(Mode::Scrollback).capabilities();
        let alt = backend(Mode::AltScreen).capabilities();
        assert!(scrollback.scrollback_passthrough);
        assert!(!alt.scrollback_passthrough);
        assert!(!alt.mouse_capture, "nothing here captures the mouse");
        assert!(!alt.search, "nothing here searches the viewport");
        assert!(scrollback.osc8_links && alt.osc8_links);
    }

    #[test]
    fn a_write_failure_is_kept_for_the_host() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut backend = TerminalBackend::new(Mode::Scrollback, Failing);
        mount_text(&mut backend, "a");
        backend.request_render(false);
        backend.render();
        let error = backend.take_error().expect("the failure is reported once");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(backend.take_error().is_none(), "taken, not repeated");
    }
}
