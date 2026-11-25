//! Cargo-pretty's inline view, with session-local mbx cache information.
//!
//! Cargo remains the only orchestrator: the original command runs once in a
//! terminal, including runners and doctests. Only its presentation is adapted.
mod model;
mod norimel;
#[cfg(test)]
mod tests;
mod upstream;
mod view;

use eyre::{Context, Result};
use mbx_cache_core::AgentStats;
use model::{Model, strip_ansi};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use ratatui::crossterm::{
    SynchronizedUpdate, cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, terminal,
};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    sync::mpsc,
    time::{Duration, Instant},
};

pub(super) fn enabled(arguments: &[String]) -> bool {
    eligible(arguments)
        && log::max_level() < log::LevelFilter::Debug
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && !crate::policy::is_ci()
        && std::env::var("TERM").as_deref() != Ok("dumb")
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("CARGO_TERM_COLOR").as_deref() != Ok("never")
        && std::env::var("CARGO_TERM_PROGRESS_WHEN").as_deref() != Ok("never")
        && terminal::size().is_ok_and(|(cols, rows)| cols >= 50 && rows >= 16)
}

/// Identify the command used by the terminal display, skipping Cargo globals
/// such as the invocation-local target configuration added by mbx.
fn cargo_verb(arguments: &[String]) -> Option<&str> {
    super::launch::cargo_subcommand(arguments)
}

fn eligible(arguments: &[String]) -> bool {
    matches!(
        cargo_verb(arguments),
        Some("build" | "b" | "check" | "c" | "clippy" | "run" | "r" | "test" | "t")
    ) && !arguments
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|arg| {
            matches!(
                arg.as_str(),
                "--quiet"
                    | "--verbose"
                    | "--help"
                    | "--version"
                    | "--message-format"
                    | "--color"
                    | "--config"
            ) || arg.starts_with("--message-format=")
                || arg.starts_with("--color=")
                || arg.starts_with("--config=")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..].contains(['q', 'v', 'h', 'V']))
        })
}

fn cargo_arguments(arguments: &[String]) -> Vec<String> {
    let mut result = arguments.to_vec();
    let index = result
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(result.len());
    result.insert(
        index,
        "--message-format=json,json-diagnostic-rendered-ansi".into(),
    );
    result
}

pub(super) fn run(
    cargo: &OsStr,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
    inspect_warnings: bool,
    stats: impl Fn() -> AgentStats,
) -> Result<Option<ExitCode>> {
    let mut started = false;
    match run_inner(
        cargo,
        arguments,
        environment,
        inspect_warnings,
        &mut started,
        stats,
    ) {
        Ok(status) => Ok(Some(status)),
        Err(error) if !started => {
            log::debug!("pretty terminal unavailable; using plain Cargo: {error}");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Replaces the builder's base environment with the caller's.
///
/// On Windows, `portable-pty` overlays the registry's machine and user
/// environments onto the process environment when it constructs a builder. That
/// drops `PATH` entries added in the calling shell, overwrites overridden
/// variables, and restores variables the shell removed, so the pretty display
/// would change what Cargo inherits compared with the plain launch.
fn seed_process_environment(
    command: &mut CommandBuilder,
    process: impl IntoIterator<Item = (OsString, OsString)>,
) {
    command.env_clear();
    for (key, value) in process {
        command.env(key, value);
    }
}

fn run_inner(
    cargo: &OsStr,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
    inspect_warnings: bool,
    started: &mut bool,
    stats: impl Fn() -> AgentStats,
) -> Result<ExitCode> {
    let (cols, rows) = terminal::size()?;
    let pair = NativePtySystem::default()
        .openpty(pty_size(cols, rows))
        .map_err(|e| eyre::eyre!(e))?;
    let mut command = CommandBuilder::new(cargo);
    if cfg!(windows) {
        seed_process_environment(&mut command, std::env::vars_os());
    }
    command.args(cargo_arguments(arguments));
    // Cargo supplies the denominator itself, without an unstable unit-graph
    // probe. Include presentation overrides in the launch overlay so native
    // applications receive the caller's environment after the build ends.
    let mut environment = environment.clone();
    environment.insert("CARGO_TERM_PROGRESS_WHEN".into(), "always".into());
    environment.insert("CARGO_TERM_PROGRESS_WIDTH".into(), cols.to_string());
    super::launch::record_overlay(&mut environment)?;
    for (key, value) in &environment {
        command.env(key, value);
    }
    command.cwd(std::env::current_dir()?);
    let mut reader = pair.master.try_clone_reader().map_err(|e| eyre::eyre!(e))?;
    let mut input = pair.master.take_writer().map_err(|e| eyre::eyre!(e))?;
    let _terminal = TerminalGuard::enter()?;
    let screen = Screen::stderr()?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| eyre::eyre!(e))?;
    *started = true;
    drop(pair.slave);
    let (send, receive) = mpsc::sync_channel(32);
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    if send.send(Ok(bytes[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = send.send(Err(error));
                    break;
                }
            }
        }
    });
    let mut renderer = Renderer::new(arguments, screen, cols, rows);
    let mut cursor_query = CursorQuery::default();
    let result = (|| -> Result<portable_pty::ExitStatus> {
        let mut exit_seen = false;
        let mut last_output = Instant::now();
        loop {
            if !exit_seen && child.try_wait().map_err(|e| eyre::eyre!(e))?.is_some() {
                exit_seen = true;
                last_output = Instant::now();
            }
            match receive.recv_timeout(renderer.poll_timeout(Instant::now())) {
                Ok(Ok(bytes)) => {
                    last_output = Instant::now();
                    let bytes = if cfg!(windows) {
                        cursor_query.answer(bytes, &mut input)?
                    } else {
                        bytes
                    };
                    renderer.feed_bytes(&bytes, &stats)?;
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Drain every available chunk, regardless of burst size. Only
                    // an idle reader after Cargo exits ends a retained-slave stream.
                    if exit_seen && last_output.elapsed() >= Duration::from_secs(1) {
                        break;
                    }
                    renderer.flush_partial()?;
                }
            }
            forward_input(&mut input)?;
            if let Ok((cols, rows)) = terminal::size() {
                let size = pty_size(cols, rows);
                if pair.master.get_size().map_err(|e| eyre::eyre!(e))? != size {
                    pair.master.resize(size).map_err(|e| eyre::eyre!(e))?;
                    renderer.resize(cols, rows)?;
                }
            }
            renderer.present(Instant::now(), &stats)?;
        }
        let held = cursor_query.take();
        if !held.is_empty() {
            renderer.feed_bytes(&held, &stats)?;
        }
        renderer.finish_stream()?;
        child.wait().map_err(|e| eyre::eyre!(e))
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = result.wrap_err("running Cargo with the pretty display")?;
    renderer.draw_summary(status.success(), &stats)?;
    if !renderer.proxy {
        let count = renderer.model.warnings.len() + renderer.model.failures.len();
        if count > 0 && (!status.success() || inspect_warnings) {
            let mut browser = view::Browser::default();
            loop {
                let block = view::render(
                    &mut renderer.model,
                    Some(&mut browser),
                    renderer.width,
                    renderer.height,
                );
                renderer.screen.draw(block)?;
                if let Event::Key(key) = event::read()?
                    && key.kind != event::KeyEventKind::Release
                {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break;
                        }
                        KeyCode::Up if browser.inspecting => {
                            browser.scroll = browser.scroll.saturating_sub(1)
                        }
                        KeyCode::Down if browser.inspecting => {
                            browser.scroll = browser.scroll.saturating_add(1)
                        }
                        KeyCode::Up => {
                            browser.selected = browser.selected.saturating_sub(1);
                            browser.scroll = 0;
                        }
                        KeyCode::Down => {
                            browser.selected = (browser.selected + 1).min(count - 1);
                            browser.scroll = 0;
                        }
                        KeyCode::Enter => {
                            browser.inspecting = !browser.inspecting;
                            browser.scroll = 0;
                        }
                        KeyCode::PageUp => browser.scroll = browser.scroll.saturating_sub(10),
                        KeyCode::PageDown => browser.scroll = browser.scroll.saturating_add(10),
                        _ => {}
                    }
                }
            }
            renderer.screen.clear()?;
        }
        for warning in &renderer.model.warnings {
            renderer.screen.diagnostic(&warning.rendered)?;
            renderer.screen.write(b"\r\n")?;
        }
        renderer.screen.commit();
    }
    Ok(ExitCode::from(status_code(&status)))
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    // ConPTY hard-wraps output at the pseudoconsole width, which splits Cargo's
    // one-line JSON messages so they no longer parse. Make it as wide as ConPTY
    // allows; the real terminal wraps the text we forward to it. Unix PTYs do
    // not wrap output, so they keep the real width.
    let cols = if cfg!(windows) { i16::MAX as u16 } else { cols };
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn status_code(status: &portable_pty::ExitStatus) -> u8 {
    #[cfg(unix)]
    if let Some(name) = status.signal() {
        // portable-pty retains the signal's platform name rather than number.
        for signal in 1..=127 {
            let description = unsafe { libc::strsignal(signal) };
            if !description.is_null()
                && unsafe { std::ffi::CStr::from_ptr(description) }.to_string_lossy() == name
            {
                return 128 + signal as u8;
            }
        }
    }
    u8::try_from(status.exit_code()).unwrap_or(1)
}

// Unix input remains byte-for-byte terminal input, including function keys,
// paste sequences, control characters and application-specific escape codes.
#[cfg(unix)]
fn forward_input(input: &mut impl Write) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } > 0 && poll.revents & libc::POLLIN != 0 {
        let mut bytes = [0u8; 8192];
        let size =
            unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len()) };
        if size > 0 {
            input.write_all(&bytes[..size as usize])?;
            input.flush()?;
        } else if size < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn forward_input(input: &mut impl Write) -> io::Result<()> {
    while event::poll(Duration::ZERO)? {
        match event::read()? {
            Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                input.write_all(&key_bytes(key))?
            }
            Event::Paste(text) => input.write_all(text.as_bytes())?,
            _ => {}
        }
    }
    input.flush()
}

#[cfg(windows)]
fn key_bytes(key: event::KeyEvent) -> Vec<u8> {
    let mut bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii() => {
            vec![(c as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![27],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![27, b'O', b'P' + n - 1],
        KeyCode::F(n @ 5..=12) => format!(
            "\x1b[{}~",
            [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]
        )
        .into_bytes(),
        _ => Vec::new(),
    };
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 27);
    }
    bytes
}

/// Answers the cursor position request ConPTY sends at startup.
///
/// ConPTY emits nothing else until the host replies, so without an answer the
/// build never starts. The request is consumed here rather than shown, since
/// the real terminal would answer it a second time into Cargo's input. Only
/// that startup request is answered; later queries, such as those from a
/// program run by `cargo run`, go to the real terminal.
#[derive(Default)]
struct CursorQuery {
    held: Vec<u8>,
    answered: bool,
}

impl CursorQuery {
    const QUERY: &'static [u8] = b"\x1b[6n";
    const REPLY: &'static [u8] = b"\x1b[1;1R";

    /// Returns the bytes held back while waiting for the rest of a request.
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    fn answer(&mut self, bytes: Vec<u8>, input: &mut impl Write) -> io::Result<Vec<u8>> {
        let mut data = std::mem::take(&mut self.held);
        data.extend(bytes);
        if self.answered {
            return Ok(data);
        }
        let mut out = Vec::with_capacity(data.len());
        let mut rest = data.as_slice();
        if let Some(at) = rest
            .windows(Self::QUERY.len())
            .position(|window| window == Self::QUERY)
        {
            out.extend_from_slice(&rest[..at]);
            rest = &rest[at + Self::QUERY.len()..];
            input.write_all(Self::REPLY)?;
            input.flush()?;
            self.answered = true;
            out.extend_from_slice(rest);
            return Ok(out);
        }
        // Hold back a trailing prefix of the request that the next read may complete.
        let keep = (1..Self::QUERY.len())
            .rev()
            .find(|&n| rest.ends_with(&Self::QUERY[..n]))
            .unwrap_or(0);
        out.extend_from_slice(&rest[..rest.len() - keep]);
        self.held = rest[rest.len() - keep..].to_vec();
        Ok(out)
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Build,
    Run,
    Test,
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
    partial: bool,
    swallow_lf: bool,
}

impl Decoder {
    fn feed<O: Output>(
        &mut self,
        bytes: &[u8],
        model: &mut Model,
        screen: &mut Screen<O>,
        mode: Mode,
        proxy: &mut bool,
        stats: &impl Fn() -> AgentStats,
    ) -> io::Result<()> {
        if *proxy {
            return screen.write(bytes);
        }
        let mut passthrough = Vec::new();
        for chunk in bytes.split_inclusive(|byte| matches!(byte, b'\n' | b'\r')) {
            if *proxy {
                screen.write(chunk)?;
                continue;
            }
            if self.swallow_lf && chunk == b"\n" {
                self.swallow_lf = false;
                continue;
            }
            self.swallow_lf = false;
            self.pending.extend_from_slice(chunk);
            if chunk.ends_with(b"\n") || chunk.ends_with(b"\r") {
                let clean = std::str::from_utf8(&self.pending).ok().map(strip_ansi);
                let line = clean
                    .as_deref()
                    .unwrap_or("")
                    .trim_end_matches(['\n', '\r']);
                let handled = !self.partial
                    && ((!model.build_finished && (model.cargo(line) || model.status(line)))
                        || (matches!(mode, Mode::Test) && model.test_line(line)));
                self.swallow_lf = handled && chunk.ends_with(b"\r");
                if !handled {
                    passthrough.extend_from_slice(&self.pending);
                }
                self.pending.clear();
                self.partial = false;
                if matches!(mode, Mode::Run) && model.build_finished && model.build_ok == Some(true)
                {
                    if !passthrough.is_empty() {
                        screen.write(&passthrough)?;
                        passthrough.clear();
                    }
                    model.update_stats(stats());
                    model.finished = Some((true, model.started.elapsed()));
                    screen.draw(view::summary(model))?;
                    screen.commit();
                    for warning in &model.warnings {
                        screen.diagnostic(&warning.rendered)?;
                        screen.write(b"\r\n")?;
                    }
                    *proxy = true;
                    screen.show_cursor()?;
                }
            } else if self.pending.len() >= 1024 * 1024 {
                if !passthrough.is_empty() {
                    screen.write(&passthrough)?;
                    passthrough.clear();
                }
                self.partial = true;
                self.flush_partial(screen, true)?;
            }
        }
        if !passthrough.is_empty() {
            screen.write(&passthrough)?;
        }
        Ok(())
    }

    fn flush_partial<O: Output>(
        &mut self,
        screen: &mut Screen<O>,
        native_output: bool,
    ) -> io::Result<()> {
        // Compiler JSON may be emitted in many writes; retain a bounded record.
        // Ordinary partial output (prompts, custom harnesses) is forwarded now.
        if !self.pending.is_empty()
            && (native_output || self.partial || !self.pending.starts_with(b"{"))
        {
            screen.write(&self.pending)?;
            self.pending.clear();
            self.partial = true;
        }
        Ok(())
    }

    fn finish<O: Output>(&mut self, screen: &mut Screen<O>) -> io::Result<()> {
        if !self.pending.is_empty() {
            screen.write(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

/// The progress block on screen and the child output waiting to be shown with it.
///
/// Every method returns the bytes to write instead of writing them, so a caller
/// can present each change in one synchronized update and tests can inspect it.
#[derive(Default)]
struct Frame {
    /// Rows of the block above the cursor, zero once it is erased or committed.
    drawn: u16,
    /// Complete lines of child output not yet shown, in arrival order.
    held: Vec<u8>,
    /// Passed-through output (written while `drawn == 0`) ended in a bare CR,
    /// so the next draw must wait for its LF or terminate the line first.
    unterminated_cr: bool,
    /// A held CR was ended with an LF of our own, so the child's LF, if it
    /// arrives, must not end the line a second time.
    lf_owed: bool,
    /// When a frame first found a trailing CR waiting. Cleared when the line
    /// ends or is shown, so the next CR gets a full grace period of its own.
    cr_since: Option<Instant>,
}

impl Frame {
    fn erase(&mut self, out: &mut Vec<u8>) {
        if self.drawn > 0 {
            out.extend_from_slice(format!("\r\x1b[{}A\x1b[J", self.drawn).as_bytes());
            self.drawn = 0;
        }
    }

    /// Small complete-line bursts wait while a block is showing so the next
    /// [`Frame::draw`] can erase, print, and redraw in one synchronized update
    /// without blinking. Once held output reaches [`HELD_LIMIT`] (or on a
    /// partial line such as a prompt), the block is erased once and subsequent
    /// mid-frame output passes straight through without redrawing the panel
    /// until the next frame interval.
    fn write(&mut self, bytes: &[u8]) -> Vec<u8> {
        let bytes = match bytes {
            [b'\n', rest @ ..] if self.lf_owed => rest,
            _ => bytes,
        };
        self.lf_owed = false;
        if bytes.is_empty() {
            return Vec::new();
        }
        self.held.extend_from_slice(bytes);
        let line_ended = matches!(bytes.last(), Some(b'\n' | b'\r'));
        if self.drawn > 0 && line_ended {
            if self.held.len() >= HELD_LIMIT && !self.held.ends_with(b"\r") {
                return self.present_passthrough();
            }
            if self.held.len() >= HELD_HARD_LIMIT {
                return self.clear();
            }
            Vec::new()
        } else {
            self.present_passthrough()
        }
    }

    /// Erase any drawn block and emit held output without redrawing the panel.
    fn present_passthrough(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.held.len());
        self.erase(&mut out);
        let trailing_cr = self.held.ends_with(b"\r");
        out.append(&mut self.held);
        self.unterminated_cr = trailing_cr;
        if !trailing_cr {
            self.cr_since = None;
        }
        out
    }

    /// Move held output to `out`. A line that ends in a bare CR is ended with an
    /// LF, or the block drawn next would print over it.
    fn flush_held(&mut self, out: &mut Vec<u8>) {
        let unterminated = self.trailing_cr();
        out.append(&mut self.held);
        self.unterminated_cr = false;
        self.cr_since = None;
        if unterminated {
            out.push(b'\n');
            self.lf_owed = true;
        }
    }

    /// Erase the block and show what was held, for a block about to be redrawn
    /// from scratch.
    fn clear(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.held.len());
        self.erase(&mut out);
        self.flush_held(&mut out);
        out
    }

    /// Whether a frame should wait for what follows a trailing CR. Drawing now
    /// would put a newline after half a CRLF and leave a blank row when the LF
    /// arrives. The wait ends [`CR_GRACE`] after it was first seen.
    fn awaiting_lf(&mut self, now: Instant) -> bool {
        if !self.trailing_cr() {
            self.cr_since = None;
            return false;
        }
        let since = *self.cr_since.get_or_insert(now);
        now.saturating_duration_since(since) < CR_GRACE
    }

    /// Output ends in a CR: either half a CRLF or a line that rewrites itself.
    fn trailing_cr(&self) -> bool {
        self.held.ends_with(b"\r") || (self.held.is_empty() && self.unterminated_cr)
    }

    /// Replace the block: erase the old one, then held output, diagnostics
    /// and the new block.
    fn draw(&mut self, diagnostics: &[u8], block: String, rows: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.held.len() + block.len());
        self.erase(&mut out);
        self.flush_held(&mut out);
        out.extend_from_slice(
            String::from_utf8_lossy(diagnostics)
                .replace("\r\n", "\n")
                .replace('\n', "\r\n")
                .as_bytes(),
        );
        out.extend_from_slice(block.as_bytes());
        self.drawn = rows;
        out
    }

    /// Leave the block in scrollback.
    fn commit(&mut self) {
        debug_assert!(self.held.is_empty(), "held output would be lost");
        self.drawn = 0;
        self.unterminated_cr = false;
        self.cr_since = None;
    }
}

/// Animation and redraw cadence for the inline progress panel.
const FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// Maximum wait between channel polls so input forwarding, terminal resize, and
/// partial-line prompt flushing stay responsive.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a trailing CR may wait for its LF before it is taken as a line
/// that rewrites itself. A CRLF split by a read boundary completes far sooner.
const CR_GRACE: Duration = Duration::from_millis(250);

/// Maximum complete-line output held while the panel stays on screen between
/// frames. Above this threshold the panel is erased once and mid-frame output
/// passes straight through until the next [`FRAME_INTERVAL`].
const HELD_LIMIT: usize = 8 * 1024;

/// Upper bound on held bytes even when output ends in a bare CR, so a flood of
/// carriage-return progress lines cannot grow memory without bound.
const HELD_HARD_LIMIT: usize = 1024 * 1024;

/// Output sink for [`Screen`], allowing unit tests to capture the exact bytes
/// (including DEC 2026 synchronized update sequences) written to the terminal.
trait Output {
    fn emit(&mut self, bytes: &[u8], synchronized: bool) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
}

fn emit_to(output: &mut impl Write, bytes: &[u8], synchronized: bool) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if synchronized {
        output.sync_update(|out| out.write_all(bytes))??;
    } else {
        output.write_all(bytes)?;
        output.flush()?;
    }
    Ok(())
}

struct StderrOutput;

impl Output for StderrOutput {
    fn emit(&mut self, bytes: &[u8], synchronized: bool) -> io::Result<()> {
        emit_to(&mut io::stderr().lock(), bytes, synchronized)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stderr(), cursor::Show)
    }
}

#[cfg(test)]
impl Output for Vec<u8> {
    fn emit(&mut self, bytes: &[u8], synchronized: bool) -> io::Result<()> {
        emit_to(self, bytes, synchronized)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self, cursor::Show)
    }
}

/// Restores terminal raw mode, synchronized-update state, and cursor visibility
/// when the pretty display exits or unwinds.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(io::stderr(), cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), terminal::EndSynchronizedUpdate, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

struct Screen<O: Output> {
    frame: Frame,
    diagnostics: Option<crate::logging::Capture>,
    output: O,
}

impl Screen<StderrOutput> {
    fn stderr() -> io::Result<Self> {
        let mut screen = Self {
            frame: Frame::default(),
            diagnostics: Some(crate::logging::Capture::start()),
            output: StderrOutput,
        };
        screen.clear()?;
        Ok(screen)
    }
}

impl<O: Output> Screen<O> {
    #[cfg(test)]
    fn with_output(output: O) -> Self {
        Self {
            frame: Frame::default(),
            diagnostics: None,
            output,
        }
    }

    /// Write one composed frame. A block that is erased or repainted goes out in
    /// a single DEC 2026 synchronized update, which unsupported terminals ignore.
    fn emit(&mut self, bytes: &[u8], synchronized: bool) -> io::Result<()> {
        self.output.emit(bytes, synchronized)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.output.show_cursor()
    }

    /// Whether a frame should wait for what follows a trailing CR.
    fn awaiting_lf(&mut self, now: Instant) -> bool {
        self.frame.awaiting_lf(now)
    }

    fn clear(&mut self) -> io::Result<()> {
        let synchronized = self.frame.drawn > 0;
        let bytes = self.frame.clear();
        self.emit(&bytes, synchronized)
    }

    fn draw(&mut self, block: norimel::Block) -> io::Result<()> {
        // Compose before touching the terminal, then present the erase, held
        // output, diagnostics and replacement together.
        let diagnostics = self
            .diagnostics
            .as_ref()
            .map(crate::logging::Capture::drain)
            .unwrap_or_default();
        let rows = block.size().1;
        let text = block.to_string().replace('\n', "\r\n") + "\r\n";
        let bytes = self.frame.draw(&diagnostics, text, rows);
        self.emit(&bytes, true)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let synchronized = self.frame.drawn > 0;
        let bytes = self.frame.write(bytes);
        self.emit(&bytes, synchronized)
    }

    // Cargo JSON contains LF-delimited diagnostic text, unlike bytes read
    // from the child PTY. Raw mode requires explicit carriage returns here.
    fn diagnostic(&mut self, text: &str) -> io::Result<()> {
        self.write(text.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes())
    }

    fn commit(&mut self) {
        self.frame.commit();
    }
}

impl<O: Output> Drop for Screen<O> {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

struct Renderer<O: Output> {
    model: Model,
    mode: Mode,
    decoder: Decoder,
    screen: Screen<O>,
    proxy: bool,
    last_frame: Option<Instant>,
    width: u16,
    height: u16,
}

impl<O: Output> Renderer<O> {
    fn new(arguments: &[String], screen: Screen<O>, cols: u16, rows: u16) -> Self {
        let mode = match cargo_verb(arguments) {
            Some("run" | "r") => Mode::Run,
            Some("test" | "t") => Mode::Test,
            _ => Mode::Build,
        };
        Self {
            model: Model::new(arguments),
            mode,
            decoder: Decoder::default(),
            screen,
            proxy: false,
            last_frame: None,
            width: block_width(cols),
            height: block_height(rows),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.width = block_width(cols);
        self.height = block_height(rows);
        self.screen.clear()
    }

    fn feed_bytes(&mut self, bytes: &[u8], stats: &impl Fn() -> AgentStats) -> io::Result<()> {
        self.decoder.feed(
            bytes,
            &mut self.model,
            &mut self.screen,
            self.mode,
            &mut self.proxy,
            stats,
        )
    }

    fn flush_partial(&mut self) -> io::Result<()> {
        self.decoder
            .flush_partial(&mut self.screen, self.model.build_finished)
    }

    fn can_draw(&mut self, now: Instant) -> bool {
        !self.proxy && !self.decoder.partial && !self.screen.awaiting_lf(now)
    }

    fn frame_due(&self, now: Instant) -> bool {
        self.last_frame
            .is_none_or(|last| now.saturating_duration_since(last) >= FRAME_INTERVAL)
    }

    /// Bounds the channel wait to the next frame deadline without spinning.
    fn poll_timeout(&mut self, now: Instant) -> Duration {
        if !self.can_draw(now) {
            return POLL_INTERVAL;
        }
        let Some(last) = self.last_frame else {
            return POLL_INTERVAL;
        };
        FRAME_INTERVAL
            .saturating_sub(now.saturating_duration_since(last))
            .min(POLL_INTERVAL)
    }

    fn present(&mut self, now: Instant, stats: &impl Fn() -> AgentStats) -> io::Result<()> {
        if self.can_draw(now) && self.frame_due(now) {
            self.model.update_stats(stats());
            self.screen
                .draw(view::render(&mut self.model, None, self.width, self.height))?;
            self.last_frame = Some(now);
        }
        Ok(())
    }

    fn finish_stream(&mut self) -> io::Result<()> {
        self.decoder.finish(&mut self.screen)
    }

    fn draw_summary(&mut self, success: bool, stats: &impl Fn() -> AgentStats) -> io::Result<()> {
        self.model.update_stats(stats());
        self.model.finished = Some((success, self.model.started.elapsed()));
        if !self.proxy {
            self.screen
                .draw(view::render(&mut self.model, None, self.width, self.height))?;
            // Full diagnostic text remains in scrollback even if the user dismisses
            // the optional browser, and the child's failure status stays authoritative.
            self.screen.commit();
            for error in &self.model.errors {
                self.screen.diagnostic(error)?;
                self.screen.write(b"\r\n")?;
            }
        }
        Ok(())
    }
}

fn block_width(cols: u16) -> u16 {
    cols.max(2).saturating_sub(1)
}

/// Rows for the progress block in a terminal `rows` tall.
///
/// A terminal that shrinks pushes the top of an inline block into scrollback,
/// where the cursor cannot reach it to erase it. Keeping the block to half the
/// screen means a shrink to a little over half the old height still fits it.
/// It never goes below [`MIN_BLOCK_HEIGHT`], the rows the layout needs to keep
/// the progress bar and cache totals in view, so short terminals are unchanged.
fn block_height(rows: u16) -> u16 {
    let rows = rows.max(3);
    (rows - 2)
        .min((rows / 2).max(MIN_BLOCK_HEIGHT))
        .clamp(1, 27)
}

/// Rows `render_content` needs to show the progress bar, cache totals and
/// elapsed time with one crate slot.
const MIN_BLOCK_HEIGHT: u16 = 14;
