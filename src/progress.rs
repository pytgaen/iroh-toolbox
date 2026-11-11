use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
    future::IntoFuture,
    io::{IsTerminal, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

static LEVEL: AtomicU8 = AtomicU8::new(0);
static TERMINAL: AtomicBool = AtomicBool::new(false);
static STATUS_SHOWN: AtomicBool = AtomicBool::new(false);
const LIVE_WIDTH: usize = 78;

/// An interactive line editor that owns the terminal: every message is printed
/// above its input line, and the live status line is kept for its prompt.
static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

struct Console {
    print: Printer,
    status: Option<String>,
}

/// Prints complete lines above an interactive input line.
pub type Printer = Box<dyn FnMut(String) -> std::io::Result<()> + Send>;

/// User-facing stderr message, below any live status line.
macro_rules! note {
    ($($arg:tt)*) => {
        $crate::progress::emit(true, format_args!($($arg)*))
    };
}

/// A refusal, failure or mistake, marked `!` in a chat.
macro_rules! alert {
    ($($arg:tt)*) => {
        $crate::progress::emit_marked($crate::progress::Mark::Alert, format_args!($($arg)*))
    };
}

/// A completed outcome, marked `✓` in a chat.
macro_rules! done {
    ($($arg:tt)*) => {
        $crate::progress::emit_marked($crate::progress::Mark::Done, format_args!($($arg)*))
    };
}

/// Diagnostic stderr message, shown from -v.
macro_rules! detail {
    ($($arg:tt)*) => {
        if $crate::progress::level() >= 1 {
            note!($($arg)*);
        }
    };
}

/// Stdout output, below any live status line when both share a terminal.
macro_rules! out {
    ($($arg:tt)*) => {
        $crate::progress::emit(false, format_args!($($arg)*))
    };
}

/// Select stderr detail once at startup:
/// - 0: necessary steps only; a terminal rewrites only the transfer line in place.
/// - 1 (-v): every step and connection diagnostics.
/// - 2 (-vv): every periodic report as its own line with raw counters.
pub fn init(verbosity: u8) {
    LEVEL.store(verbosity.min(2), Ordering::Relaxed);
    TERMINAL.store(std::io::stderr().is_terminal(), Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

/// Whether a transfer line may be rewritten in place.
pub fn live() -> bool {
    level() < 2 && TERMINAL.load(Ordering::Relaxed)
}

/// Rewrite the live status line in place.
pub fn draw_status(line: &str) {
    let mut line: String = safe_text(line).chars().take(LIVE_WIDTH).collect();
    if line.chars().count() == LIVE_WIDTH {
        line.pop();
        line.push('…');
    }
    STATUS_SHOWN.store(true, Ordering::Relaxed);
    if let Some(console) = CONSOLE.lock().unwrap().as_mut() {
        console.status = Some(line);
        return;
    }
    eprint!("\r\x1b[2K{line}");
    let _ = std::io::stderr().flush();
}

/// Keep the live status line on screen and move below it before other output.
pub fn keep_status() {
    emit_with(&mut CONSOLE.lock().unwrap(), None);
}

/// Print one line on stderr (`note!`) or stdout (`out!`), above the input
/// line of an interactive console.
pub fn emit(stderr: bool, args: fmt::Arguments) {
    let line = if stderr {
        Line::Note(Mark::Info, args.to_string(), true)
    } else {
        Line::Out(safe_text(&args.to_string()))
    };
    emit_with(&mut CONSOLE.lock().unwrap(), Some(line));
}

/// Print one stderr line with its chat mark (`alert!`, `done!`).
pub fn emit_marked(mark: Mark, args: fmt::Arguments) {
    emit_with(
        &mut CONSOLE.lock().unwrap(),
        Some(Line::Note(mark, args.to_string(), true)),
    );
}

/// Print one marked stderr line whose first word is kept as written, such as
/// a machine name.
pub fn emit_verbatim(mark: Mark, text: &str) {
    emit_with(
        &mut CONSOLE.lock().unwrap(),
        Some(Line::Note(mark, text.to_owned(), false)),
    );
}

/// A received chat message on stdout: `[HH:MM ]WHO> TEXT`.
pub fn said(who: &str, text: &str) {
    let style = chat_style();
    let stamp = style.map(|style| style.stamp()).unwrap_or_default();
    let (who, text) = (safe_text(who), safe_text(text));
    let line = match style {
        // The prompt's colors: reedline paints `HH:MM ` light green.
        Some(style) if style.color && !stamp.is_empty() => {
            format!(
                "{}{} {text}",
                paint("92", &stamp),
                paint("95", &format!("{who}>"))
            )
        }
        Some(style) if style.color => format!("{} {text}", paint("95", &format!("{who}>"))),
        _ => format!("{stamp}{who}> {text}"),
    };
    emit_with(&mut CONSOLE.lock().unwrap(), Some(Line::Out(line)));
}

/// What a line says, which sets its mark in a chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Info,
    Alert,
    Done,
}

enum Line {
    /// Stderr text, escaped when printed; in a chat, capitalized unless false.
    Note(Mark, String, bool),
    /// Stdout text, already escaped.
    Out(String),
}

/// How a chat shows its stderr lines; `None` outside a chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatStyle {
    pub timestamps: bool,
    pub color: bool,
}

const STYLED: u8 = 1;
const STAMPED: u8 = 2;
const COLORED: u8 = 4;
static STYLE: AtomicU8 = AtomicU8::new(0);

/// Marks stderr lines as chat notes until the guard drops.
pub fn style_chat(style: ChatStyle) -> ChatStyleGuard {
    let bits =
        STYLED | if style.timestamps { STAMPED } else { 0 } | if style.color { COLORED } else { 0 };
    STYLE.store(bits, Ordering::Relaxed);
    ChatStyleGuard
}

pub struct ChatStyleGuard;

impl Drop for ChatStyleGuard {
    fn drop(&mut self) {
        STYLE.store(0, Ordering::Relaxed);
    }
}

fn chat_style() -> Option<ChatStyle> {
    let bits = STYLE.load(Ordering::Relaxed);
    (bits & STYLED != 0).then_some(ChatStyle {
        timestamps: bits & STAMPED != 0,
        color: bits & COLORED != 0,
    })
}

impl ChatStyle {
    fn stamp(self) -> String {
        if self.timestamps {
            chrono::Local::now().format("%H:%M ").to_string()
        } else {
            String::new()
        }
    }

    /// `[HH:MM ]MARK Text`, colored by its mark.
    fn note(self, mark: Mark, text: &str) -> String {
        self.note_as(mark, &capitalized(text))
    }

    fn note_as(self, mark: Mark, text: &str) -> String {
        let (symbol, color) = match mark {
            Mark::Info => ('·', "90"),
            Mark::Alert => ('!', "33"),
            Mark::Done => ('✓', "32"),
        };
        let line = format!("{}{symbol} {text}", self.stamp());
        if self.color {
            paint(color, &line)
        } else {
            line
        }
    }
}

/// Colors are wanted unless `NO_COLOR` is set to a non-empty value
/// (<https://no-color.org>) or the terminal is `dumb`.
pub fn color_wanted() -> bool {
    color_wanted_with(
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

fn color_wanted_with(no_color: Option<&std::ffi::OsStr>, term: Option<&std::ffi::OsStr>) -> bool {
    no_color.is_none_or(|value| value.is_empty()) && term.is_none_or(|term| term != "dumb")
}

fn paint(color: &str, text: &str) -> String {
    format!("\x1b[{color}m{text}\x1b[0m")
}

/// `Peer`, `Connecting to sender`: the first letter in upper case.
pub fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn emit_with(console: &mut Option<Console>, line: Option<Line>) {
    // Whatever a message embeds (a close reason, a peer or file name) must not
    // drive the terminal or fake a line of its own: one call prints one line,
    // so newlines are escaped like every other control character, before a
    // chat adds its own colors.
    let style = chat_style();
    let note = |mark, text: &str, capital: bool| {
        let text = safe_text(text);
        match style {
            Some(style) if capital => style.note(mark, &text),
            Some(style) => style.note_as(mark, &text),
            None => text,
        }
    };
    let line = line.map(|line| match line {
        Line::Note(mark, text, capital) => (true, note(mark, &text, capital)),
        Line::Out(text) => (false, text),
    });
    let shown = STATUS_SHOWN.swap(false, Ordering::Relaxed);
    if let Some(console) = console.as_mut() {
        let status = console
            .status
            .take()
            .filter(|_| shown)
            .map(|status| note(Mark::Info, &status, true));
        let text = match (status, line) {
            (Some(status), Some((_, line))) => format!("{status}\n{line}\n"),
            (Some(status), None) => format!("{status}\n"),
            (None, Some((_, line))) => format!("{line}\n"),
            (None, None) => return,
        };
        if (console.print)(text.clone()).is_err() {
            // The editor is gone: the message is still shown.
            eprint!("{text}");
        }
        return;
    }
    if shown {
        eprintln!();
    }
    match line {
        Some((true, line)) => eprintln!("{line}"),
        Some((false, line)) => println!("{line}"),
        None => {}
    }
}

/// Route output through an interactive line editor until [`detach_console`].
pub fn attach_console(print: Printer) {
    *CONSOLE.lock().unwrap() = Some(Console {
        print,
        status: None,
    });
}

pub fn detach_console() {
    let console = CONSOLE.lock().unwrap().take();
    if console.is_some_and(|console| console.status.is_some()) {
        STATUS_SHOWN.store(false, Ordering::Relaxed);
    }
}

fn console_attached() -> bool {
    CONSOLE.lock().unwrap().is_some()
}

/// Takes the live status line of an interactive console, to show it in
/// another form than the line it would otherwise print.
pub fn take_console_status() -> Option<String> {
    let status = CONSOLE.lock().unwrap().as_mut()?.status.take();
    if status.is_some() {
        STATUS_SHOWN.store(false, Ordering::Relaxed);
    }
    status
}

/// The live status line of an interactive console, printed periodically.
pub fn console_status() -> Option<String> {
    CONSOLE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|console| console.status.clone())
}

pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1048576.0)
}

/// A human-readable size: `512 B`, `12.3 KiB`, `1.2 GiB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `1 file`, `12 files`.
pub fn file_count(count: usize) -> String {
    match count {
        1 => "1 file".to_owned(),
        count => format!("{count} files"),
    }
}

/// A human-readable duration: `< 0.1 s`, `0.3 s`, `42 s`, `2 min 05 s`, `1 h 02 min`.
pub fn duration(value: Duration) -> String {
    let seconds = value.as_secs_f64();
    if seconds < 0.1 {
        return "< 0.1 s".to_owned();
    }
    if seconds < 10.0 {
        return format!("{seconds:.1} s");
    }
    let seconds = value.as_secs();
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {:02} s", seconds / 60, seconds % 60),
        _ => format!("{} h {:02} min", seconds / 3600, seconds % 3600 / 60),
    }
}

/// Step reporting on stderr: `step` is shown by default, `phase` from -v.
/// A step with a total shows a percentage, rewritten in place on a terminal
/// and kept on screen once the step ends.
pub struct Progress {
    enabled: bool,
    live: bool,
    label: &'static str,
    phase: Cow<'static, str>,
    info: bool,
    started: Instant,
    reported: Instant,
    bytes: u64,
    total: Option<u64>,
    printed: HashSet<String>,
    drawing: bool,
}

impl Progress {
    pub fn new(label: &'static str, enabled: bool) -> Self {
        Self {
            enabled,
            live: true,
            label,
            phase: Cow::Borrowed("starting"),
            info: false,
            started: Instant::now(),
            reported: Instant::now(),
            bytes: 0,
            total: None,
            printed: HashSet::new(),
            drawing: false,
        }
    }

    /// Never rewrites a status line: chat input shares the terminal.
    /// Steps as lines, never rewritten in place, unless an interactive
    /// console shows the live line.
    pub fn lines(label: &'static str, enabled: bool) -> Self {
        let mut progress = Self::new(label, enabled);
        progress.live = false;
        progress
    }

    /// A step the user needs by default.
    pub fn step(&mut self, step: impl Into<Cow<'static, str>>, total: Option<u64>) {
        self.start(step.into(), total, true);
    }

    /// An internal phase, shown from -v.
    pub fn phase(&mut self, phase: impl Into<Cow<'static, str>>, total: Option<u64>) {
        self.start(phase.into(), total, false);
    }

    fn start(&mut self, phase: Cow<'static, str>, total: Option<u64>, info: bool) {
        self.finish();
        self.phase = phase;
        self.info = info;
        self.bytes = 0;
        self.total = total;
        self.started = Instant::now();
        self.render(true);
    }

    pub fn update(&mut self, bytes: u64) {
        self.bytes = bytes;
        let interval = if level() >= 2 { 1000 } else { 200 };
        if self.reported.elapsed() >= Duration::from_millis(interval) {
            self.report();
        }
    }

    pub fn report_bytes(&mut self, bytes: u64) {
        self.bytes = bytes;
        self.report();
    }

    pub fn report(&mut self) {
        self.render(false);
    }

    fn percent(&self, total: u64) -> f64 {
        if total == 0 {
            100.0
        } else {
            (self.bytes as f64 / total as f64 * 100.0).min(100.0)
        }
    }

    fn render(&mut self, new_phase: bool) {
        self.reported = Instant::now();
        if !self.enabled {
            return;
        }
        if level() >= 2 {
            let elapsed = self.started.elapsed().as_secs_f64();
            match self.total {
                Some(total) => note!(
                    "{}: {} {}/{} bytes ({:.0}%) elapsed={elapsed:.1}s",
                    self.label,
                    self.phase,
                    self.bytes,
                    total,
                    self.percent(total)
                ),
                None => note!(
                    "{}: {} bytes={} elapsed={elapsed:.1}s",
                    self.label,
                    self.phase,
                    self.bytes
                ),
            }
            return;
        }
        if !self.info && level() == 0 {
            return;
        }
        let Some(total) = self.total else {
            if new_phase && self.printed.insert(self.phase.to_string()) {
                note!("{}", self.phase);
            }
            return;
        };
        // A line-mode report shares the terminal with chat input; an
        // interactive console keeps its live line apart from that input.
        if (self.live || console_attached()) && live() {
            self.draw(total);
        } else if new_phase && self.printed.insert(self.phase.to_string()) {
            note!("{} ({})", self.phase, size(total));
        }
    }

    fn draw(&mut self, total: u64) {
        draw_status(&format!(
            "{} {:.0}% ({}/{})",
            self.phase,
            self.percent(total),
            size(self.bytes),
            size(total)
        ));
        self.drawing = true;
    }

    /// Leave a finished transfer line on screen with its final count.
    pub fn finish(&mut self) {
        if !self.drawing {
            return;
        }
        if let Some(total) = self.total {
            self.draw(total);
        }
        self.drawing = false;
        keep_status();
    }

    pub async fn wait<T>(&mut self, future: impl IntoFuture<Output = T>) -> T {
        let future = future.into_future();
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        loop {
            tokio::select! {
                result = &mut future => return result,
                _ = tick.tick() => self.report(),
            }
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Escapes terminal control and bidirectional characters of remote text.
pub fn safe_text(value: &str) -> String {
    escape(value)
}

/// Text of a local file or tool (a TOML parse error) whose line layout is
/// worth keeping. Never wrap text that came from the network in it.
#[derive(Debug)]
pub struct Local(pub String);

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Local {}

/// The lines of an error chain: every cause escaped, and the layout of
/// [`Local`] causes kept as lines of their own.
pub fn error_lines(error: &anyhow::Error, redact: impl Fn(String) -> String) -> Vec<String> {
    let mut lines = vec![String::new()];
    for (index, cause) in error.chain().enumerate() {
        if index > 0 {
            lines.last_mut().unwrap().push_str(": ");
        }
        let text = redact(cause.to_string());
        if cause.is::<Local>() {
            for (number, line) in text.split('\n').enumerate() {
                if number > 0 {
                    lines.push(String::new());
                }
                lines.last_mut().unwrap().push_str(&safe_text(line));
            }
        } else {
            lines.last_mut().unwrap().push_str(&safe_text(&text));
        }
    }
    lines
}

/// Prints `error: …` for an error chain, one `note!` per line.
pub fn print_error(error: &anyhow::Error, redact: impl Fn(String) -> String) {
    for (number, line) in error_lines(error, redact).into_iter().enumerate() {
        if number == 0 {
            note!("error: {line}");
        } else {
            note!("{line}");
        }
    }
}

fn escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            let hostile =
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if hostile {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, time::Duration};

    use super::{ChatStyle, Mark};

    #[test]
    fn chat_notes_carry_a_mark_a_capital_and_optional_colors() {
        let plain = ChatStyle {
            timestamps: false,
            color: false,
        };
        assert_eq!(
            plain.note(Mark::Info, "connecting to sender"),
            "· Connecting to sender"
        );
        assert_eq!(
            plain.note(Mark::Alert, "unknown command /x"),
            "! Unknown command /x"
        );
        assert_eq!(plain.note(Mark::Done, "received a"), "✓ Received a");
        let colored = ChatStyle {
            timestamps: false,
            color: true,
        };
        assert_eq!(colored.note(Mark::Alert, "no"), "\x1b[33m! No\x1b[0m");
        let stamped = ChatStyle {
            timestamps: true,
            color: false,
        };
        let line = stamped.note(Mark::Info, "ready");
        assert!(
            line.ends_with(" · Ready") && line.len() == "HH:MM · Ready".len(),
            "{line}"
        );
    }

    #[test]
    fn no_color_and_dumb_terminals_disable_colors() {
        let no_color = |value: &str| super::color_wanted_with(Some(OsStr::new(value)), None);
        assert!(super::color_wanted_with(None, Some(OsStr::new("xterm"))));
        assert!(no_color(""), "an empty NO_COLOR is ignored");
        assert!(!no_color("1"));
        assert!(!super::color_wanted_with(None, Some(OsStr::new("dumb"))));
    }

    #[test]
    fn sizes_durations_and_counts_read_naturally() {
        assert_eq!(super::size(336), "336 B");
        assert_eq!(super::size(1536), "1.5 KiB");
        assert_eq!(super::duration(Duration::from_millis(20)), "< 0.1 s");
        assert_eq!(super::duration(Duration::from_millis(340)), "0.3 s");
        assert_eq!(super::duration(Duration::from_secs(42)), "42 s");
        assert_eq!(super::duration(Duration::from_secs(125)), "2 min 05 s");
        assert_eq!(super::duration(Duration::from_secs(3720)), "1 h 02 min");
        assert_eq!(super::file_count(1), "1 file");
        assert_eq!(super::file_count(312), "312 files");
        assert_eq!(super::capitalized("élan"), "Élan");
    }

    #[test]
    fn local_text_keeps_its_lines_and_remote_text_does_not() {
        let local = anyhow::Error::new(super::Local("parse error\n  |\n2 | bad".into()))
            .context("invalid peer book /x");
        let lines = super::error_lines(&local, |text| text);
        assert_eq!(
            lines,
            ["invalid peer book /x: parse error", "  |", "2 | bad"]
        );
        let remote = anyhow::anyhow!("closed: x\nreceived 1 file").context("peer");
        assert_eq!(super::error_lines(&remote, |t| t).len(), 1);
    }

    #[test]
    fn output_escapes_every_control_including_newlines() {
        assert_eq!(
            super::safe_text("a\x1b[2Jb\r\u{9b}c\u{202e}\nd"),
            "a\\u{1b}[2Jb\\r\\u{9b}c\\u{202e}\\nd"
        );
    }

    #[test]
    fn notes_and_errors_escape_hostile_remote_text() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&seen);
        super::attach_console(Box::new(move |text| {
            sink.lock().unwrap().push_str(&text);
            Ok(())
        }));
        let reason = "\x1b[2J\x1b]0;pwned\x07\r\u{9b}31m";
        note!("error: connection closed: {reason}");
        super::draw_status(&format!("receiving {reason}"));
        detail!("hidden");
        let forged = "x\nreceived 1 file, BLAKE3 verified";
        note!("error: {forged}");
        super::detach_console();
        let seen = seen.lock().unwrap();
        assert!(
            seen.contains("error: connection closed: \\u{1b}[2J\\u{1b}]0;pwned\\u{7}\\r\\u{9b}31m")
        );
        assert!(seen.contains("error: x\\nreceived 1 file, BLAKE3 verified\n"));
        assert_eq!(seen.matches("BLAKE3").count(), 1);
        assert_eq!(
            seen.lines().filter(|l| l.starts_with("received")).count(),
            0
        );
        assert!(!seen.contains(['\x1b', '\x07', '\r', '\u{9b}']), "{seen:?}");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(super::size(0), "0 B");
        assert_eq!(super::size(1023), "1023 B");
        assert_eq!(super::size(1024), "1.0 KiB");
        assert_eq!(super::size(1_288_490_189), "1.2 GiB");
        assert_eq!(super::size(u64::MAX), "16384.0 PiB");
    }
}
