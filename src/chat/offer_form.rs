//! `chat --auto-cmd`: a form that answers the peer's file offer. It pauses
//! the line editor, draws itself below the chat, and gives back the chat
//! command it builds (`/receive [--force] DESTINATION` or `/reject`), which
//! the conversation runs like a typed line.
//!
//! While the form is open, lines printed by the chat are shown above it, then
//! handed back to the line editor, which prints them for good above its prompt.
use std::{
    io::{self, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    terminal::{self, ClearType},
};
use reedline::ExternalOutput;
use unicode_width::UnicodeWidthChar;

use super::{PROMPT, encode_path};
use crate::progress::safe_text;

/// The destination offered first; the participant may change it.
const DESTINATION: &str = ".";
/// Columns of the destination field.
const FIELD_WIDTH: usize = 24;
const HINT: &str = "Tab: next · Space: tick · Enter: choose · Esc: later";

/// Lines for the terminal: printed by the line editor, or kept while a form
/// covers it.
struct Output {
    receiver: mpsc::Receiver<String>,
    held: Vec<String>,
}

/// The line editor's [`ExternalOutput`], shared with the form.
#[derive(Clone)]
pub(super) struct SharedOutput(Arc<Mutex<Output>>);

impl SharedOutput {
    pub(super) fn new(receiver: mpsc::Receiver<String>) -> Self {
        Self(Arc::new(Mutex::new(Output {
            receiver,
            held: Vec::new(),
        })))
    }

    /// Lines received since the last call, held ones first.
    fn take(&self) -> Vec<String> {
        let mut output = self.0.lock().unwrap();
        let mut lines = std::mem::take(&mut output.held);
        lines.extend(output.receiver.try_iter());
        lines
    }

    /// Prints what the line editor will not: the chat ended during a form.
    pub(super) fn print_rest(&self) {
        let mut out = io::stderr().lock();
        for text in self.take() {
            let _ = writeln!(out, "{}", text.trim_end_matches('\n'));
        }
        let _ = out.flush();
    }

    /// Gives lines back, to be printed before any newer one.
    fn hold(&self, mut lines: Vec<String>) {
        let mut output = self.0.lock().unwrap();
        lines.append(&mut output.held);
        output.held = lines;
    }
}

impl ExternalOutput for SharedOutput {
    fn drain(&mut self) -> io::Result<Vec<String>> {
        Ok(self.take())
    }
}

/// What the conversation asks of the editor thread.
pub(super) enum Control {
    Open { id: u64 },
    Close { id: u64 },
}

/// Opens and closes forms from the conversation.
#[derive(Clone)]
pub(super) struct Forms {
    pub(super) tx: mpsc::Sender<Control>,
    /// The line editor's break signal: it pauses the editor for the form.
    pub(super) wake: Arc<AtomicBool>,
}

impl Forms {
    pub(super) fn open(&self, id: u64) {
        if self.tx.send(Control::Open { id }).is_ok() {
            self.wake.store(true, Ordering::Relaxed);
        }
    }

    /// Closes the form of offer `id`, if it is still open.
    pub(super) fn close(&self, id: u64) {
        let _ = self.tx.send(Control::Close { id });
    }
}

/// The offer whose form should be open after these requests, if any.
pub(super) fn pending(controls: &mpsc::Receiver<Control>, mut open: Option<u64>) -> Option<u64> {
    for control in controls.try_iter() {
        match control {
            Control::Open { id } => open = Some(id),
            Control::Close { id } if open == Some(id) => open = None,
            Control::Close { .. } => {}
        }
    }
    open
}

/// How a form ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// A chat command to run as if typed.
    Command(String),
    /// Esc: the offer stays pending.
    Later,
    /// The offer is gone: withdrawn, expired, or the connection was lost.
    Closed,
    /// Ctrl+C.
    Interrupted,
    /// The chat is ending.
    Ending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Destination,
    Force,
    Accept,
    Refuse,
}

impl Focus {
    const ORDER: [Self; 4] = [Self::Destination, Self::Force, Self::Accept, Self::Refuse];

    fn step(self, forward: bool) -> Self {
        let at = Self::ORDER.iter().position(|focus| *focus == self).unwrap();
        let len = Self::ORDER.len();
        Self::ORDER[if forward { at + 1 } else { at + len - 1 } % len]
    }
}

/// The form's state, apart from the terminal.
#[derive(Debug)]
struct Form {
    destination: Vec<char>,
    /// Position of the cursor in `destination`, in characters.
    cursor: usize,
    force: bool,
    focus: Focus,
    error: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        let destination: Vec<char> = DESTINATION.chars().collect();
        Self {
            cursor: destination.len(),
            destination,
            force: false,
            focus: Focus::Destination,
            error: None,
        }
    }
}

impl Form {
    /// Applies one key; returns how the form ends, if it does.
    fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if control => return Some(Outcome::Interrupted),
            KeyCode::Esc => return Some(Outcome::Later),
            KeyCode::Tab | KeyCode::Down => self.focus = self.focus.step(true),
            KeyCode::BackTab | KeyCode::Up => self.focus = self.focus.step(false),
            KeyCode::Enter => match self.focus {
                Focus::Destination | Focus::Force => self.focus = self.focus.step(true),
                Focus::Accept | Focus::Refuse => return self.choose(),
            },
            _ => match self.focus {
                Focus::Destination => self.edit(key.code, control),
                Focus::Force if key.code == KeyCode::Char(' ') => self.force = !self.force,
                Focus::Force => {}
                Focus::Accept | Focus::Refuse => match key.code {
                    KeyCode::Char(' ') => return self.choose(),
                    KeyCode::Left => self.focus = Focus::Accept,
                    KeyCode::Right => self.focus = Focus::Refuse,
                    _ => {}
                },
            },
        }
        None
    }

    fn edit(&mut self, code: KeyCode, control: bool) {
        match code {
            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::Char('e') if control => self.cursor = self.destination.len(),
            KeyCode::Char('u') if control => {
                self.destination.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char(c) if !control && !c.is_control() => {
                self.destination.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.destination.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.destination.len() => {
                self.destination.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.destination.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.destination.len(),
            _ => return,
        }
        self.error = None;
    }

    /// Enter or Space on a button.
    fn choose(&mut self) -> Option<Outcome> {
        if self.focus == Focus::Refuse {
            return Some(Outcome::Command("/reject".to_owned()));
        }
        match self.command() {
            Ok(command) => Some(Outcome::Command(command)),
            Err(error) => {
                self.error = Some(error);
                self.focus = Focus::Destination;
                None
            }
        }
    }

    /// `/receive [--force] DESTINATION`, once the destination can receive.
    fn command(&self) -> Result<String, String> {
        let destination: String = self.destination.iter().collect();
        if destination.trim().is_empty() {
            return Err("type the folder that receives the offer".to_owned());
        }
        check_destination(Path::new(&destination))?;
        // `--` ends the options: a destination that starts with `--` is a path.
        let separator = if destination.starts_with("--") {
            "-- "
        } else {
            ""
        };
        Ok(format!(
            "/receive {}{separator}{}",
            if self.force { "--force " } else { "" },
            encode_path(&destination, true)
        ))
    }

    /// The form's lines: the command, the buttons, then an error if any.
    fn lines(&self, color: bool) -> Vec<String> {
        let focused = |focus: Focus, text: &str| {
            if self.focus == focus {
                format!("\x1b[7m{text}\x1b[27m")
            } else {
                text.to_owned()
            }
        };
        let force = if self.force { "[x]" } else { "[ ]" };
        let hint = if color {
            format!("\x1b[90m{HINT}\x1b[0m")
        } else {
            HINT.to_owned()
        };
        let mut lines = vec![
            format!(
                "┌ /receive [{}]  {} --force",
                self.field(),
                focused(Focus::Force, force)
            ),
            format!(
                "└ {}  {}   {hint}",
                focused(Focus::Accept, "[ accept ]"),
                focused(Focus::Refuse, "[ refuse ]")
            ),
        ];
        if let Some(error) = &self.error {
            let error = format!("  ! {}", safe_text(error));
            lines.push(if color {
                format!("\x1b[33m{error}\x1b[0m")
            } else {
                error
            });
        }
        lines
    }

    /// The destination in its field, scrolled to show the cursor, which is
    /// drawn in reverse video while the field has the focus.
    fn field(&self) -> String {
        let columns =
            |chars: &[char]| -> usize { chars.iter().map(|c| c.width().unwrap_or(0)).sum() };
        let editing = self.focus == Focus::Destination;
        let mut start = 0;
        while start < self.cursor && columns(&self.destination[start..self.cursor]) >= FIELD_WIDTH {
            start += 1;
        }
        let mut field = String::new();
        let mut used = 0;
        for (at, c) in self.destination.iter().enumerate().skip(start) {
            let width = c.width().unwrap_or(0);
            if used + width > FIELD_WIDTH {
                break;
            }
            used += width;
            if editing && at == self.cursor {
                field.push_str(&format!("\x1b[7m{c}\x1b[27m"));
            } else {
                field.push(*c);
            }
        }
        if used < FIELD_WIDTH {
            if editing && self.cursor == self.destination.len() {
                field.push_str("\x1b[7m \x1b[27m");
            } else {
                field.push(' ');
            }
            used += 1;
        }
        field.push_str(&" ".repeat(FIELD_WIDTH - used));
        field
    }
}

/// The checks of a reception's destination, worded for the form: an existing
/// folder, or a new name in one.
fn check_destination(path: &Path) -> Result<(), String> {
    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(&absolute) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!("{}: not a folder", absolute.display()));
        }
        return Ok(());
    }
    match absolute.parent() {
        Some(parent) if parent.is_dir() => Ok(()),
        Some(parent) => Err(format!("{}: no such folder", parent.display())),
        None => Err(format!("{}: no such folder", absolute.display())),
    }
}

/// The part of `line` that fits in `columns`, its color sequences kept.
fn fit(line: &str, columns: usize) -> String {
    let mut fitted = String::new();
    let mut used = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            fitted.push(c);
            if chars.peek() == Some(&'[') {
                fitted.push(chars.next().unwrap());
                for c in chars.by_ref() {
                    fitted.push(c);
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        let width = c.width().unwrap_or(0);
        if used + width > columns {
            fitted.push_str("\x1b[0m");
            break;
        }
        used += width;
        fitted.push(c);
    }
    fitted
}

/// The rows below the chat that the form draws on.
struct Screen {
    top: u16,
}

impl Screen {
    /// Draws `lines` from the top row, scrolling the terminal when they reach
    /// its bottom; only the last lines are drawn when they exceed its height.
    fn draw(&mut self, lines: &[String]) -> io::Result<()> {
        // As the line editor does, a terminal of unknown size has 80×24.
        let (columns, rows) = match terminal::size()? {
            (0, 0) => (80, 24),
            (columns, rows) => (columns, rows.max(1)),
        };
        let shown = &lines[lines.len().saturating_sub(usize::from(rows))..];
        let mut out = io::stderr().lock();
        queue!(
            out,
            cursor::MoveTo(0, self.top),
            terminal::Clear(ClearType::FromCursorDown)
        )?;
        for (at, line) in shown.iter().enumerate() {
            if at > 0 {
                out.write_all(b"\r\n")?;
            }
            out.write_all(fit(line, usize::from(columns.max(2)) - 1).as_bytes())?;
        }
        let count = shown.len() as u16;
        if self.top + count > rows {
            self.top = rows - count;
        }
        out.flush()
    }

    /// Erases the form, leaving the cursor where the editor's prompt goes.
    fn clear(&self) -> io::Result<()> {
        let mut out = io::stderr().lock();
        queue!(
            out,
            cursor::MoveTo(0, self.top),
            terminal::Clear(ClearType::FromCursorDown)
        )?;
        out.flush()
    }
}

/// The form's terminal mode, restored when it ends, errors included.
struct Raw;

impl Raw {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let raw = Self;
        let mut out = io::stderr().lock();
        queue!(out, cursor::Hide)?;
        out.flush()?;
        Ok(raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let mut out = io::stderr().lock();
        let _ = queue!(out, cursor::Show);
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// How the chat shows its lines.
#[derive(Clone, Copy)]
pub(super) struct Style {
    pub(super) timestamps: bool,
    pub(super) color: bool,
}

impl Style {
    /// The line the form gives, shown as if typed at the prompt.
    fn typed(self, command: &str) -> String {
        let stamp = if self.timestamps {
            chrono::Local::now().format("%H:%M ").to_string()
        } else {
            String::new()
        };
        format!("{stamp}{PROMPT}{command}")
    }
}

/// Runs the form of offer `id` on the terminal until it ends. The lines the
/// chat prints meanwhile show above it, then go back to `output`, followed by
/// the command when the form gives one.
pub(super) fn run(
    id: u64,
    controls: &mpsc::Receiver<Control>,
    output: &SharedOutput,
    ending: &AtomicBool,
    style: Style,
    poll: Duration,
) -> io::Result<Outcome> {
    let raw = Raw::enter()?;
    // A terminal that does not tell where its cursor is gets the form at its
    // bottom, as the line editor does.
    let top = match cursor::position() {
        Ok((_, row)) => row,
        Err(_) => terminal::size()?.1.saturating_sub(1),
    };
    let mut screen = Screen { top };
    let mut form = Form::default();
    let mut printed: Vec<String> = Vec::new();
    let mut open = Some(id);
    let mut dirty = true;
    let result = loop {
        if ending.load(Ordering::Relaxed) {
            break Ok(Outcome::Ending);
        }
        open = pending(controls, open);
        if open != Some(id) {
            break Ok(Outcome::Closed);
        }
        let lines = output.take();
        if !lines.is_empty() {
            printed.extend(lines);
            dirty = true;
        }
        if dirty {
            let mut shown: Vec<String> = printed
                .iter()
                .flat_map(|text| text.split('\n'))
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            shown.extend(form.lines(style.color));
            if let Err(error) = screen.draw(&shown) {
                break Err(error);
            }
            dirty = false;
        }
        match event::poll(poll) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => break Err(error),
        }
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                if let Some(outcome) = form.key(key) {
                    break Ok(outcome);
                }
                dirty = true;
            }
            Ok(Event::Resize(..)) => dirty = true,
            Ok(_) => {}
            Err(error) => break Err(error),
        }
    };
    let cleared = screen.clear();
    drop(raw);
    if let Ok(Outcome::Command(command)) = &result {
        printed.push(style.typed(command));
    }
    output.hold(printed);
    cleared?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(form: &mut Form, code: KeyCode) -> Option<Outcome> {
        form.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(form: &mut Form, text: &str) {
        for c in text.chars() {
            press(form, KeyCode::Char(c));
        }
    }

    fn replace_destination(form: &mut Form, path: &Path) {
        form.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(form, path.to_str().unwrap());
    }

    #[test]
    fn accepting_the_defaults_receives_here_without_force() {
        let mut form = Form::default();
        // Enter moves on from the field and the box: a stray Enter accepts nothing.
        assert_eq!(press(&mut form, KeyCode::Enter), None);
        assert_eq!(press(&mut form, KeyCode::Enter), None);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Some(Outcome::Command("/receive .".to_owned()))
        );
    }

    #[test]
    fn the_box_and_the_field_change_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("holiday photos");
        let mut form = Form::default();
        replace_destination(&mut form, &destination);
        press(&mut form, KeyCode::Tab);
        press(&mut form, KeyCode::Char(' '));
        press(&mut form, KeyCode::Tab);
        let Some(Outcome::Command(command)) = press(&mut form, KeyCode::Char(' ')) else {
            panic!("the form should give a command");
        };
        let (path, replacement, hidden) =
            super::super::receive_arguments(command.strip_prefix("/receive").unwrap()).unwrap();
        assert_eq!(path, destination);
        assert_eq!(replacement, crate::files::Replacement::Force);
        assert!(!hidden);
    }

    #[test]
    fn refuse_rejects_and_escape_or_ctrl_c_end_the_form() {
        let mut form = Form::default();
        press(&mut form, KeyCode::BackTab);
        assert_eq!(
            press(&mut form, KeyCode::Enter),
            Some(Outcome::Command("/reject".to_owned()))
        );
        assert_eq!(
            press(&mut Form::default(), KeyCode::Esc),
            Some(Outcome::Later)
        );
        assert_eq!(
            Form::default().key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Outcome::Interrupted)
        );
    }

    #[test]
    fn a_missing_folder_keeps_the_form_open_with_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut form = Form::default();
        replace_destination(&mut form, &dir.path().join("a/b"));
        press(&mut form, KeyCode::BackTab);
        press(&mut form, KeyCode::BackTab);
        assert_eq!(form.focus, Focus::Accept);
        assert_eq!(press(&mut form, KeyCode::Enter), None);
        assert_eq!(form.focus, Focus::Destination);
        assert!(form.error.as_deref().unwrap().ends_with("no such folder"));
        assert_eq!(form.lines(false).len(), 3);
        // Editing clears the error; moving the focus does not.
        press(&mut form, KeyCode::Tab);
        assert!(form.error.is_some());
        press(&mut form, KeyCode::BackTab);
        press(&mut form, KeyCode::Backspace);
        assert!(form.error.is_none());
    }

    #[test]
    fn a_destination_that_starts_like_an_option_stays_a_path() {
        // A new name in the current folder.
        let path = Path::new("--odd");
        let mut form = Form::default();
        replace_destination(&mut form, path);
        let command = form.command().unwrap();
        assert_eq!(command, "/receive -- --odd");
        let (parsed, ..) =
            super::super::receive_arguments(command.strip_prefix("/receive").unwrap()).unwrap();
        assert_eq!(parsed, path);
    }

    #[test]
    fn requests_settle_on_the_last_open_offer() {
        let (tx, rx) = mpsc::channel();
        tx.send(Control::Open { id: 1 }).unwrap();
        tx.send(Control::Close { id: 1 }).unwrap();
        tx.send(Control::Open { id: 2 }).unwrap();
        tx.send(Control::Close { id: 1 }).unwrap();
        assert_eq!(pending(&rx, None), Some(2));
        tx.send(Control::Close { id: 2 }).unwrap();
        assert_eq!(pending(&rx, Some(2)), None);
    }

    #[test]
    fn held_lines_come_back_before_newer_ones() {
        let (tx, rx) = mpsc::sync_channel(4);
        let mut output = SharedOutput::new(rx);
        tx.send("a".to_owned()).unwrap();
        let taken = output.take();
        tx.send("c".to_owned()).unwrap();
        output.hold([taken, vec!["b".to_owned()]].concat());
        assert_eq!(output.drain().unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn lines_are_cut_to_the_terminal_width_with_their_colors() {
        assert_eq!(fit("\x1b[90mabcdef\x1b[0m", 3), "\x1b[90mabc\x1b[0m");
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("日本語", 5), "日本\x1b[0m");
    }

    #[test]
    fn the_field_scrolls_to_keep_the_cursor_visible() {
        let mut form = Form::default();
        type_text(&mut form, &"x".repeat(40));
        let field = form.field();
        assert!(field.ends_with("x\x1b[7m \x1b[27m"));
        assert!(!field.contains('.'));
        press(&mut form, KeyCode::Home);
        assert!(form.field().starts_with("\x1b[7m.\x1b[27mx"));
    }
}
