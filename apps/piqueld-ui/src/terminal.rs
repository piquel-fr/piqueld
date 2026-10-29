//! Terminal emulation for dashboard exec sessions: parsing command lines,
//! encoding keystrokes, and rendering the screen as HTML.
use piqueld_client::exec::{ExecCommand, ExecCommandError, TerminalSize};
use std::fmt::Write as _;
use vt100::Color;

/// Lines of history kept above the screen.
const SCROLLBACK: usize = 5000;

/// A rejected command line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommandLineError {
    /// A quote or escape is not closed.
    #[error("command has an unclosed quote or a trailing backslash")]
    Unbalanced,
    /// The line names no program.
    #[error(transparent)]
    Empty(#[from] ExecCommandError),
}

/// Splits a command line into a program and its arguments with POSIX shell
/// quoting rules, without running a shell: `psql -c 'select 1'`.
///
/// # Errors
/// Returns [`CommandLineError`] for unbalanced quoting or an empty line.
pub fn parse_command(line: &str) -> Result<ExecCommand, CommandLineError> {
    let words = shlex::split(line).ok_or(CommandLineError::Unbalanced)?;
    Ok(ExecCommand::parse(words)?)
}

/// A key press, as the browser reports it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Key<'a> {
    /// `KeyboardEvent.key`: a printable character or a named key such as `ArrowUp`.
    pub key: &'a str,
    /// Control is held.
    pub ctrl: bool,
    /// Alt (Option) is held.
    pub alt: bool,
    /// Shift is held.
    pub shift: bool,
}

impl Key<'_> {
    /// The bytes a terminal sends for this key, or `None` for keys left to the
    /// browser: Ctrl+Shift+V, which pastes, Ctrl+Shift+C, which terminals use
    /// to copy and which must not interrupt the command as Ctrl+C would, and
    /// keys with no terminal encoding. `application_cursor` selects the cursor key mode
    /// full-screen programs request.
    #[must_use]
    pub fn encode(&self, application_cursor: bool) -> Option<Vec<u8>> {
        if self.ctrl && self.shift && matches!(self.key, "C" | "c" | "V" | "v") {
            return None;
        }
        // xterm's modifier parameter: 1 plus Shift 1, Alt 2, Control 4.
        let modifiers = 1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        let cursor = |code: char| {
            if modifiers > 1 {
                format!("\x1b[1;{modifiers}{code}").into_bytes()
            } else if application_cursor {
                format!("\x1bO{code}").into_bytes()
            } else {
                format!("\x1b[{code}").into_bytes()
            }
        };
        let tilde = |code: u8| format!("\x1b[{code}~").into_bytes();
        let bytes = match self.key {
            "ArrowUp" => return Some(cursor('A')),
            "ArrowDown" => return Some(cursor('B')),
            "ArrowRight" => return Some(cursor('C')),
            "ArrowLeft" => return Some(cursor('D')),
            "Home" => return Some(cursor('H')),
            "End" => return Some(cursor('F')),
            "Enter" => b"\r".to_vec(),
            "Backspace" => vec![0x7f],
            "Tab" if self.shift => b"\x1b[Z".to_vec(),
            "Tab" => b"\t".to_vec(),
            "Escape" => vec![0x1b],
            "Insert" => tilde(2),
            "Delete" => tilde(3),
            "PageUp" => tilde(5),
            "PageDown" => tilde(6),
            "F1" => b"\x1bOP".to_vec(),
            "F2" => b"\x1bOQ".to_vec(),
            "F3" => b"\x1bOR".to_vec(),
            "F4" => b"\x1bOS".to_vec(),
            "F5" => tilde(15),
            "F6" => tilde(17),
            "F7" => tilde(18),
            "F8" => tilde(19),
            "F9" => tilde(20),
            "F10" => tilde(21),
            "F11" => tilde(23),
            "F12" => tilde(24),
            key => {
                let mut chars = key.chars();
                let (Some(char), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                if self.ctrl {
                    vec![control(char)?]
                } else {
                    char.to_string().into_bytes()
                }
            }
        };
        // Alt sends Escape first, as xterm's `metaSendsEscape` does.
        Some(if self.alt {
            [&[0x1b], &bytes[..]].concat()
        } else {
            bytes
        })
    }
}

/// The control character Ctrl+`char` sends.
fn control(char: char) -> Option<u8> {
    Some(match char {
        'a'..='z' | 'A'..='Z' => char.to_ascii_uppercase() as u8 & 0x1f,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Answers the status queries programs send to their terminal, such as the
/// cursor position line editors request, which `vt100` leaves to callbacks.
#[derive(Default)]
struct Replies(Vec<u8>);

impl vt100::Callbacks for Replies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let param = params.first().and_then(|param| param.first()).copied();
        match (i1, c, param) {
            // Device status: "OK".
            (None, 'n', Some(5)) => self.0.extend_from_slice(b"\x1b[0n"),
            // Cursor position, 1-based. A cursor waiting to wrap past the
            // last column, which `vt100` reports as the width, is still in it.
            (None, 'n', Some(6)) => {
                let (row, col) = screen.cursor_position();
                let col = col.min(screen.size().1.saturating_sub(1));
                self.0
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // Primary device attributes: a VT100 with advanced video.
            (None, 'c', None | Some(0)) => self.0.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

/// The screen of one exec session.
pub struct Terminal {
    parser: vt100::Parser<Replies>,
    /// Output comes from a pseudo-terminal, which already ends lines with CRLF.
    tty: bool,
}

impl Terminal {
    /// An empty screen of `size`.
    #[must_use]
    pub fn new(size: TerminalSize, tty: bool) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                size.height,
                size.width,
                SCROLLBACK,
                Replies::default(),
            ),
            tty,
        }
    }

    /// Writes command output and returns the terminal's replies to queries
    /// in it, which belong on the command's input. Without a pseudo-terminal,
    /// nothing translates line feeds, so each also returns the cursor to the
    /// line's start.
    #[must_use]
    pub fn write(&mut self, data: &[u8]) -> Vec<u8> {
        if self.tty {
            self.parser.process(data);
        } else {
            for line in data.split_inclusive(|&byte| byte == b'\n') {
                self.parser.process(line);
                if line.ends_with(b"\n") {
                    self.parser.process(b"\r");
                }
            }
        }
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }

    /// Current screen size.
    #[must_use]
    pub fn size(&self) -> TerminalSize {
        let (height, width) = self.parser.screen().size();
        TerminalSize { width, height }
    }

    /// Changes the screen size. Returns whether it changed.
    pub fn resize(&mut self, size: TerminalSize) -> bool {
        let changed = self.size() != size;
        if changed {
            self.parser.screen_mut().set_size(size.height, size.width);
        }
        changed
    }

    /// Scrolls the view `lines` lines back into history (negative: forward).
    /// Returns whether the view moved.
    pub fn scroll(&mut self, lines: isize) -> bool {
        let screen = self.parser.screen_mut();
        let before = screen.scrollback();
        screen.set_scrollback(before.saturating_add_signed(lines));
        screen.scrollback() != before
    }

    /// Returns to the live screen and encodes `key`; see [`Key::encode`].
    pub fn key(&mut self, key: &Key<'_>) -> Option<Vec<u8>> {
        let bytes = key.encode(self.parser.screen().application_cursor())?;
        self.parser.screen_mut().set_scrollback(0);
        Some(bytes)
    }

    /// Returns to the live screen and encodes pasted text, bracketed when the
    /// program asked for it so it is not run as typed commands. Bracketed text
    /// loses its escape characters, so it cannot end the bracket early.
    pub fn paste(&mut self, text: &str) -> Vec<u8> {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(0);
        // Terminals send Enter as a carriage return.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if screen.bracketed_paste() {
            let text = text.replace('\x1b', "");
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        }
    }

    /// The visible screen as HTML: one line per row, with `span`s classed for
    /// colored or emphasized text and the cursor (see `.exec-screen` in
    /// `components.css`).
    #[must_use]
    pub fn html(&self) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let cursor =
            (!screen.hide_cursor() && screen.scrollback() == 0).then(|| screen.cursor_position());
        let mut html = String::new();
        for row in 0..rows {
            // Consecutive cells with the same style share one span.
            let mut run = (String::new(), String::new());
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let classes = classes(cell, cursor == Some((row, col)));
                if classes != run.0 {
                    push_run(&mut html, &run);
                    run = (classes, String::new());
                }
                match cell.contents() {
                    "" => run.1.push(' '),
                    text => escape(&mut run.1, text),
                }
            }
            push_run(&mut html, &run);
            html.push('\n');
        }
        html
    }
}

/// Appends one run of text, in a span when it is styled.
fn push_run(html: &mut String, (classes, text): &(String, String)) {
    if classes.is_empty() {
        html.push_str(text);
    } else {
        let _ = write!(html, r#"<span class="{classes}">{text}</span>"#);
    }
}

/// Escapes text for HTML element content.
fn escape(html: &mut String, text: &str) {
    for char in text.chars() {
        match char {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            char => html.push(char),
        }
    }
}

/// CSS classes for a cell; empty for default text. Inverse video and the
/// cursor swap the foreground and background. The dashboard's content security
/// policy forbids inline styles, so the stylesheet defines every class.
fn classes(cell: &vt100::Cell, cursor: bool) -> String {
    let (mut fg, mut bg) = (
        Paint::of(cell.fgcolor(), Paint::Foreground),
        Paint::of(cell.bgcolor(), Paint::Background),
    );
    if cell.inverse() != cursor {
        std::mem::swap(&mut fg, &mut bg);
    }
    let mut classes = Vec::new();
    if fg != Paint::Foreground {
        classes.push(format!("tf-{}", fg.name()));
    }
    if bg != Paint::Background {
        classes.push(format!("tb-{}", bg.name()));
    }
    for (set, class) in [
        (cell.bold(), "t-bold"),
        (cell.dim(), "t-dim"),
        (cell.italic(), "t-italic"),
        (cell.underline(), "t-underline"),
    ] {
        if set {
            classes.push(class.to_owned());
        }
    }
    classes.join(" ")
}

/// A color the stylesheet defines: the terminal's default foreground or
/// background, or one of the 16 named colors, which follow the theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Paint {
    Foreground,
    Background,
    Named(u8),
}

/// xterm's 16 named colors, for matching other colors to the nearest one.
const NAMED: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

impl Paint {
    /// Resolves a cell color, with `default` for [`Color::Default`]. The rest
    /// of the 256-color palette and RGB colors show as the nearest named color.
    fn of(color: Color, default: Self) -> Self {
        match color {
            Color::Default => default,
            Color::Idx(index @ 0..16) => Self::Named(index),
            Color::Idx(index) => Self::nearest(Self::indexed(index)),
            Color::Rgb(red, green, blue) => Self::nearest([red, green, blue]),
        }
    }

    /// RGB of an entry of xterm's 256-color palette past the named colors:
    /// a 6×6×6 cube, then a gray ramp.
    fn indexed(index: u8) -> [u8; 3] {
        let level = |value: u8| if value == 0 { 0 } else { 55 + 40 * value };
        match index {
            16..232 => {
                let index = index - 16;
                [level(index / 36), level(index / 6 % 6), level(index % 6)]
            }
            _ => [8 + 10 * index.saturating_sub(232); 3],
        }
    }

    /// The named color closest to `rgb`.
    fn nearest(rgb: [u8; 3]) -> Self {
        let distance = |named: &[u8; 3]| -> u32 {
            named
                .iter()
                .zip(rgb)
                .map(|(&a, b)| u32::from(a.abs_diff(b)).pow(2))
                .sum()
        };
        let index = (0..16u8)
            .min_by_key(|&index| distance(&NAMED[usize::from(index)]))
            .unwrap_or(0);
        Self::Named(index)
    }

    /// Class name suffix: `fg`, `bg`, or the color's number.
    fn name(self) -> String {
        match self {
            Self::Foreground => "fg".into(),
            Self::Background => "bg".into(),
            Self::Named(index) => index.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: TerminalSize = TerminalSize {
        width: 10,
        height: 2,
    };

    #[test]
    fn command_lines_split_like_a_shell() {
        let command = parse_command(r#"psql -c 'select 1' "a b"\ c"#).unwrap();
        assert_eq!(command.as_slice(), ["psql", "-c", "select 1", "a b c"]);
        assert_eq!(parse_command("sh -c 'x"), Err(CommandLineError::Unbalanced));
        assert!(matches!(
            parse_command("  "),
            Err(CommandLineError::Empty(_))
        ));
    }

    #[test]
    fn keys_encode_like_xterm() {
        let key = |key, ctrl, alt| Key {
            key,
            ctrl,
            alt,
            ..Key::default()
        };
        assert_eq!(key("c", true, false).encode(false), Some(vec![3]));
        assert_eq!(key("x", false, true).encode(false), Some(b"\x1bx".to_vec()));
        assert_eq!(key("é", false, false).encode(false), Some("é".into()));
        assert_eq!(
            key("ArrowUp", false, false).encode(false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key("ArrowUp", false, false).encode(true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key("ArrowLeft", true, false).encode(true),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(key("Shift", false, false).encode(false), None);
        let copy = Key {
            shift: true,
            ..key("C", true, false)
        };
        assert_eq!(copy.encode(false), None);
    }

    #[test]
    fn output_renders_as_escaped_styled_html() {
        let mut terminal = Terminal::new(SIZE, true);
        let _ = terminal.write(b"\x1b[31m<a>\x1b[0m&\x1b[?25l");
        assert_eq!(
            terminal.html(),
            "<span class=\"tf-1\">&lt;a&gt;</span>&amp;      \n          \n"
        );
    }

    #[test]
    fn colors_beyond_the_named_ones_show_as_the_nearest() {
        let mut terminal = Terminal::new(SIZE, true);
        // 256-color red, RGB green on default, then the cursor in inverse video.
        let _ = terminal.write(b"\x1b[38;5;196ma\x1b[38;2;0;250;10mb\x1b[0m");
        assert_eq!(
            terminal.html().lines().next().unwrap(),
            r#"<span class="tf-9">a</span><span class="tf-10">b</span><span class="tf-bg tb-fg"> </span>       "#
        );
    }

    #[test]
    fn output_without_a_terminal_returns_to_the_line_start() {
        let mut terminal = Terminal::new(SIZE, false);
        let _ = terminal.write(b"ab\ncd");
        assert!(terminal.html().starts_with("ab        \ncd"));
    }

    #[test]
    fn queries_are_answered_even_when_split_across_output() {
        let mut terminal = Terminal::new(SIZE, true);
        assert_eq!(terminal.write(b"ab\x1b["), b"");
        assert_eq!(terminal.write(b"6n"), b"\x1b[1;3R");
        assert_eq!(terminal.write(b"\x1b[c"), b"\x1b[?1;2c");
        // A full line leaves the cursor waiting to wrap, still in the last column.
        assert_eq!(terminal.write(b"\r1234567890\x1b[6n"), b"\x1b[1;10R");
    }

    #[test]
    fn pastes_are_bracketed_on_request() {
        let mut terminal = Terminal::new(SIZE, true);
        assert_eq!(terminal.paste("a\nb"), b"a\rb");
        let _ = terminal.write(b"\x1b[?2004h");
        assert_eq!(terminal.paste("a"), b"\x1b[200~a\x1b[201~");
        // A pasted end marker would run the rest as typed commands.
        assert_eq!(
            terminal.paste("a\x1b[201~\nrm"),
            b"\x1b[200~a[201~\rrm\x1b[201~"
        );
    }
}
