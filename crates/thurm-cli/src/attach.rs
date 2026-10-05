//! Render a subscribed pane in a local terminal with exclusive input control.

use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

use thurm_client::{Client, ConnectOptions};
use thurm_proto::{CursorShape, Event, Frame, PaneId, PaneSize, Request, cell_flags as flags};
use thurm_term::{ClientView, EngineConfig, TermMode, Terminal};

const PREFIX: u8 = 0x1d;
const CLEANUP: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?7h\x1b[?1l\x1b>\x1b[?2004l\x1b[0 q\x1b[?1049l";

/// Ctrl-] d detaches. Ctrl-] Ctrl-] sends one literal Ctrl-].
#[derive(Default)]
struct Escape {
    pending: bool,
}
impl Escape {
    fn input(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let mut output = Vec::with_capacity(data.len());
        for &b in data {
            if self.pending {
                self.pending = false;
                if b == b'd' {
                    return (output, true);
                }
                output.push(PREFIX);
                if b != PREFIX {
                    output.push(b);
                }
            } else if b == PREFIX {
                self.pending = true;
            } else {
                output.push(b);
            }
        }
        (output, false)
    }
}

struct LocalTerminal {
    saved: libc::termios,
    signals: Vec<signal_hook::SigId>,
}
impl LocalTerminal {
    fn enter(stop: Arc<AtomicUsize>) -> io::Result<Self> {
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut guard = Self {
            saved,
            signals: Vec::new(),
        };
        for signal in [
            libc::SIGHUP,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
        ] {
            guard.signals.push(signal_hook::flag::register_usize(
                signal,
                stop.clone(),
                signal as usize,
            )?);
        }
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        io::stdout().write_all(b"\x1b[?1049h\x1b[?7l\x1b[2J")?;
        io::stdout().flush()?;
        Ok(guard)
    }
}
impl Drop for LocalTerminal {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
        let _ = io::stdout().write_all(CLEANUP);
        let _ = io::stdout().flush();
        for signal in self.signals.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
    }
}

struct Lease {
    client: Arc<Client>,
    pane: PaneId,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.client.request_timeout(
            Request::DetachTerminal { pane: self.pane },
            Some(Duration::from_secs(2)),
        );
    }
}

fn size() -> io::Result<PaneSize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(io::Error::other("terminal size is zero"));
    }
    Ok(PaneSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
        cell_width: (ws.ws_xpixel / ws.ws_col).max(1),
        cell_height: (ws.ws_ypixel / ws.ws_row).max(1),
    })
}

pub fn run(pane: PaneId, remote: Option<&str>, json: bool) -> super::R {
    if json {
        return Err("attach does not support --json".into());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("attach needs a terminal on stdin and stdout; use ssh -t for SSH".into());
    }
    if std::env::var("TERM").is_ok_and(|v| v == "dumb") {
        return Err("attach needs an ANSI terminal".into());
    }
    if remote.is_none()
        && std::env::var(thurm_proto::ENV_PANE_ID)
            .ok()
            .and_then(|p| p.parse::<PaneId>().ok())
            == Some(pane)
    {
        return Err("cannot attach a pane to itself; use another terminal".into());
    }
    let socket = if let Some(name) = remote {
        // Check the configured host and tunnel before opening the event connection.
        drop(super::remote::connect_remote(name)?);
        thurm_config::remote_socket_path(name)
    } else {
        thurm_config::socket_path()
    };
    let (tx, rx) = mpsc::sync_channel(256);
    let overflow = Arc::new(AtomicBool::new(false));
    let full = overflow.clone();
    let client = Client::connect(
        ConnectOptions {
            socket,
            spawn_daemon: None,
            client_name: "thurm-attach",
            ui: false,
        },
        move |ev| {
            if matches!(&ev, Event::Attach { pane: p, .. } | Event::Output { pane: p, .. } | Event::Resized { pane: p, .. } | Event::PaneExited { pane: p, .. } | Event::PaneClosed { pane: p } if *p == pane)
                && tx.try_send(ev).is_err()
            {
                full.store(true, Ordering::Relaxed);
            }
        },
        || {},
    )?;
    let mut current_size = size()?;
    client.request_timeout(
        Request::AttachTerminal {
            pane,
            size: current_size,
        },
        Some(Duration::from_secs(5)),
    )?;
    let lease = Lease { client, pane };
    eprintln!("Attached to pane {pane}. Press Ctrl-] then d to detach.");
    let stop = Arc::new(AtomicUsize::new(0));
    let terminal = LocalTerminal::enter(stop.clone())?;
    let result = interact(
        &lease.client,
        pane,
        &rx,
        &overflow,
        &stop,
        &mut current_size,
    );
    drop(terminal);
    drop(lease);
    result
}

fn interact(
    client: &Client,
    pane: PaneId,
    rx: &mpsc::Receiver<Event>,
    overflow: &AtomicBool,
    stop: &AtomicUsize,
    current_size: &mut PaneSize,
) -> super::R {
    let mut model: Option<Terminal> = None;
    let mut view = ClientView::new();
    let mut escape = Escape::default();
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        let signal = stop.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(ExitCode::from((128 + signal) as u8));
        }
        if overflow.load(Ordering::Relaxed) {
            return Err("terminal output exceeded the input queue; attach again".into());
        }
        if !client.is_alive() {
            return Err("connection to thurmd lost".into());
        }
        let mut done = false;
        // Bound each batch so input and termination signals remain responsive.
        for ev in rx.try_iter().take(128) {
            match ev {
                Event::Attach { size, state, .. } => {
                    let mut term = Terminal::new(size, EngineConfig::default());
                    term.set_reads_media(false);
                    term.replay(&state);
                    model = Some(term);
                    view.invalidate();
                }
                Event::Output { data, .. } => {
                    if let Some(term) = &mut model {
                        term.advance(&data);
                    }
                }
                Event::Resized { size, .. } => {
                    if let Some(term) = &mut model {
                        term.resize(size);
                        view.invalidate();
                    }
                }
                Event::PaneExited { .. } | Event::PaneClosed { .. } => {
                    done = true;
                }
                _ => {}
            }
        }
        if let Some(term) = &mut model {
            term.flush_sync();
            // The daemon answers terminal queries. Never send a second answer from this view.
            term.drain_events();
            if let Some(snapshot) = term.snapshot(pane, &mut view) {
                render(&snapshot.frame, &mut stdout)?;
                stdout.flush()?;
            }
        }
        if done {
            return Ok(ExitCode::SUCCESS);
        }
        let latest = size()?;
        if latest != *current_size {
            client.request_timeout(
                Request::Resize { pane, size: latest },
                Some(Duration::from_secs(2)),
            )?;
            *current_size = latest;
        }
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, 30) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if ready > 0 {
            if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(ExitCode::SUCCESS);
            }
            let mut input = [0u8; 8192];
            let count = stdin.read(&mut input)?;
            if count == 0 {
                return Ok(ExitCode::SUCCESS);
            }
            let (data, detach) = escape.input(&input[..count]);
            if !data.is_empty() {
                client
                    .request_timeout(Request::Input { pane, data }, Some(Duration::from_secs(2)))?;
            }
            if detach {
                return Ok(ExitCode::SUCCESS);
            }
        }
    }
}

fn render(frame: &Frame, out: &mut impl Write) -> io::Result<()> {
    write!(out, "\x1b[?25l")?;
    if frame.full {
        write!(out, "\x1b[0m\x1b[2J")?;
    }
    if frame.shift != 0 && !frame.full {
        write!(
            out,
            "\x1b[{}{}",
            frame.shift.unsigned_abs(),
            if frame.shift < 0 { 'S' } else { 'T' }
        )?;
    }
    for row in &frame.lines {
        write!(out, "\x1b[{};1H", row.row + 1)?;
        for (col, cell) in row.cells.iter().enumerate() {
            if cell.flags & flags::WIDE_SPACER != 0 {
                continue;
            }
            write!(
                out,
                "\x1b[0;38;2;{};{};{};48;2;{};{};{}m",
                (cell.fg >> 16) & 255,
                (cell.fg >> 8) & 255,
                cell.fg & 255,
                (cell.bg >> 16) & 255,
                (cell.bg >> 8) & 255,
                cell.bg & 255
            )?;
            for (flag, code) in [
                (flags::BOLD, 1),
                (flags::DIM, 2),
                (flags::ITALIC, 3),
                (flags::ANY_UNDERLINE, 4),
                (flags::HIDDEN, 8),
                (flags::STRIKEOUT, 9),
            ] {
                if cell.flags & flag != 0 {
                    write!(out, "\x1b[{code}m")?;
                }
            }
            if let Some((_, cluster)) = row.clusters.iter().find(|(c, _)| *c as usize == col) {
                for c in cluster.chars() {
                    write!(out, "{}", if c.is_control() { ' ' } else { c })?;
                }
            } else {
                write!(out, "{}", if cell.ch.is_control() { ' ' } else { cell.ch })?;
            }
        }
    }
    let modes = TermMode::from_bits_truncate(frame.modes);
    for (mode, code) in [(TermMode::APP_CURSOR, 1), (TermMode::BRACKETED_PASTE, 2004)] {
        write!(
            out,
            "\x1b[?{code}{}",
            if modes.contains(mode) { 'h' } else { 'l' }
        )?;
    }
    write!(
        out,
        "\x1b{}",
        if modes.contains(TermMode::APP_KEYPAD) {
            '='
        } else {
            '>'
        }
    )?;
    write!(
        out,
        "\x1b[0m\x1b[{};{}H",
        frame.cursor.row + 1,
        frame.cursor.col + 1
    )?;
    if frame.cursor.shape != CursorShape::Hidden {
        let shape = match frame.cursor.shape {
            CursorShape::Underline => 4,
            CursorShape::Beam => 6,
            _ => 2,
        };
        write!(
            out,
            "\x1b[{} q\x1b[?25h",
            shape - u8::from(frame.cursor.blinking)
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detach_across_input_chunks_and_literal_prefix() {
        let mut escape = Escape::default();
        assert_eq!(escape.input(b"hello\x1d"), (b"hello".to_vec(), false));
        assert_eq!(escape.input(b"dignored"), (vec![], true));
        assert_eq!(
            escape.input(b"\x1d\x1d\x1dx"),
            (b"\x1d\x1dx".to_vec(), false)
        );
    }
    #[test]
    fn render_existing_state_and_later_output() {
        let size = PaneSize {
            cols: 30,
            rows: 6,
            ..Default::default()
        };
        let mut daemon = Terminal::new(size, EngineConfig::default());
        daemon.advance(b"old screen\x1b[?1049h\x1b[2;4H\x1b[31mwide: \xe7\x95\x8c");
        let mut local = Terminal::new(size, EngineConfig::default());
        local.replay(&daemon.serialize_state());
        let mut host = Terminal::new(size, EngineConfig::default());
        let mut view = ClientView::new();
        for update in [b"".as_slice(), b"\x1b[4;1Hnext\x1b[?25l", b"\x1b[?1049l"] {
            daemon.advance(update);
            local.advance(update);
            let frame = local.snapshot(1, &mut view).unwrap().frame;
            let mut ansi = b"\x1b[?7l".to_vec();
            render(&frame, &mut ansi).unwrap();
            host.advance(&ansi);
            let text = |t: &Terminal| {
                t.screen_text()
                    .split('\n')
                    .map(str::trim_end)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            };
            assert_eq!(text(&host), text(&daemon));
            let host_frame = host.snapshot(1, &mut ClientView::new()).unwrap().frame;
            assert_eq!(host_frame.cursor.row, frame.cursor.row);
            assert_eq!(host_frame.cursor.col, frame.cursor.col);
            assert_eq!(host_frame.cursor.shape, frame.cursor.shape);
        }
    }
}
