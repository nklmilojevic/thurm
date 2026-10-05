//! Run the attachment CLI against an isolated socket and a test PTY.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thurm_proto::{Envelope, Event, Request, Response, ServerMessage, codec};

fn terminal_flags(file: &File) -> libc::tcflag_t {
    let mut term = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(file.as_raw_fd(), &mut term) }, 0);
    term.c_lflag
}

fn exercise(stop: u8) {
    let dir = std::env::temp_dir().join(format!("thurm-attach-pty-{}-{stop}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = mpsc::channel();
    let (resize_tx, resize_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        while let Ok(Some(frame)) = codec::read_frame(&mut stream) {
            let env: Envelope = codec::decode(&frame).unwrap();
            let response = match env.request {
                Request::Hello { .. } => Response::Hello {
                    version: thurm_proto::PROTOCOL_VERSION,
                    daemon_pid: 1,
                    restored: false,
                    build: "test".into(),
                    capabilities: vec![],
                },
                Request::AttachTerminal { pane, size } => {
                    stream
                        .write_all(
                            &codec::encode(&ServerMessage::Event(Event::Attach {
                                pane,
                                size,
                                state: b"previous screen\x1b[2;1H".to_vec(),
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                    Response::Ok
                }
                Request::Input { data, .. } => {
                    tx.send(data).unwrap();
                    if stop == 2 {
                        break;
                    }
                    Response::Ok
                }
                Request::DetachTerminal { .. } => Response::Ok,
                Request::Resize { size, .. } => {
                    resize_tx.send(size).unwrap();
                    Response::Ok
                }
                other => panic!("unexpected request: {other:?}"),
            };
            stream
                .write_all(
                    &codec::encode(&ServerMessage::Response {
                        id: env.id,
                        result: Ok(response),
                    })
                    .unwrap(),
                )
                .unwrap();
        }
    });
    let mut master = 0;
    let mut slave = 0;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 640,
        ws_ypixel: 384,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        },
        0
    );
    let mut master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let before = terminal_flags(&slave);
    let mut child = Command::new(env!("CARGO_BIN_EXE_thurm"))
        .args(["attach", "--pane", "7"])
        .env("THURM_SOCKET", &socket)
        .env_remove("THURM_PANE_ID")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "attachment did not render");
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 100) } > 0 {
            let mut buf = [0; 8192];
            let n = master.read(&mut buf).unwrap();
            output.extend_from_slice(&buf[..n]);
            // Each cell has an ANSI style. The final cursor command ends the first frame.
            if output.ends_with(b"\x1b[2 q\x1b[?25h") || output.ends_with(b"\x1b[1 q\x1b[?25h") {
                break;
            }
        }
    }
    let mut host = thurm_term::Terminal::new(
        thurm_proto::PaneSize {
            cols: 80,
            rows: 24,
            ..Default::default()
        },
        thurm_term::EngineConfig::default(),
    );
    host.advance(b"\x1b[3;12r\x1b[?6h\x1b[4h\x1b(0");
    host.advance(&output);
    assert!(host.screen_text().starts_with("previous screen"));
    let frame = host
        .snapshot(7, &mut thurm_term::ClientView::new())
        .unwrap()
        .frame;
    assert_eq!((frame.cursor.row, frame.cursor.col), (1, 0));
    assert_eq!(terminal_flags(&slave) & (libc::ICANON | libc::ECHO), 0);
    size.ws_col = 96;
    size.ws_row = 28;
    assert_eq!(
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    let resized = resize_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!((resized.cols, resized.rows), (96, 28));
    master.write_all(b"hello\r").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), b"hello\r");
    if stop == 1 {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
    } else if stop == 0 {
        master.write_all(b"\x1dd").unwrap();
    }
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(match stop {
                    1 => 143,
                    2 => 1,
                    _ => 0,
                })
            );
            break;
        }
        assert!(Instant::now() < deadline, "attachment did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The kernel can set PENDIN when canonical input is restored.
    assert_eq!(
        terminal_flags(&slave) & !libc::PENDIN,
        before & !libc::PENDIN
    );
    server.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn detach_restores_terminal() {
    exercise(0);
}
#[test]
fn termination_restores_terminal() {
    exercise(1);
}

#[test]
fn disconnect_restores_terminal() {
    exercise(2);
}
