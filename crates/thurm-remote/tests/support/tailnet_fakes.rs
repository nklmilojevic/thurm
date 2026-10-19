//! Stand-ins for a tailnet host, shared by the tests that reach one: a daemon that answers
//! Hello, the WebSocket gateway `thurm-tailnet` serves in front of it, and a dial socket that
//! "dials the peer" with plain TCP.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use futures_util::{SinkExt, StreamExt};
use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

/// A stand-in daemon on `path`: answers Hello on every connection, then whatever else with Ok.
pub fn daemon(path: &Path) {
    let ln = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for conn in ln.incoming() {
            let Ok(mut conn) = conn else { break };
            std::thread::spawn(move || {
                while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut conn) {
                    let result = match env.request {
                        Request::Hello { .. } => Ok(Response::Hello {
                            version: thurm_proto::PROTOCOL_VERSION,
                            daemon_pid: std::process::id(),
                            restored: false,
                            build: thurm_proto::BUILD.into(),
                            capabilities: thurm_proto::CAPABILITIES
                                .iter()
                                .map(|c| (*c).to_owned())
                                .collect(),
                        }),
                        _ => Ok(Response::Ok),
                    };
                    let msg = ServerMessage::Response { id: env.id, result };
                    if codec::write_message(&mut conn, &msg).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// The gateway: `/v1/connect` relayed to the daemon at `daemon`, or with `refuse` an HTTP 403
/// with that text, as an admission refusal is. Returns its port and a switch that stops it.
pub fn gateway(
    daemon: PathBuf,
    refuse: Option<&'static str>,
) -> (u16, tokio::sync::watch::Sender<bool>) {
    let ln = rt().block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let port = ln.local_addr().unwrap().port();
    let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
    rt().spawn(async move {
        loop {
            let (mut tcp, _) = tokio::select! {
                a = ln.accept() => match a { Ok(a) => a, Err(_) => break },
                _ = stop.changed() => break,
            };
            let daemon = daemon.clone();
            let mut stop = stop.clone();
            tokio::spawn(async move {
                if let Some(text) = refuse {
                    let mut buf = [0u8; 4096];
                    let _ = tcp.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    );
                    let _ = tcp.write_all(resp.as_bytes()).await;
                    return;
                }
                let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let Ok(unix) = tokio::net::UnixStream::connect(&daemon).await else {
                    return;
                };
                let (mut sink, mut stream) = ws.split();
                let (mut rd, mut wr) = unix.into_split();
                let down = async {
                    let mut buf = vec![0u8; 64 * 1024];
                    while let Ok(n) = rd.read(&mut buf).await {
                        if n == 0
                            || sink
                                .send(Message::Binary(buf[..n].to_vec().into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                };
                let up = async {
                    while let Some(Ok(msg)) = stream.next().await {
                        if let Message::Binary(b) = msg
                            && wr.write_all(&b).await.is_err()
                        {
                            break;
                        }
                    }
                };
                tokio::select! {
                    _ = down => {}
                    _ = up => {}
                    _ = stop.changed() => {}
                }
            });
        }
    });
    (port, stop_tx)
}

/// The stand-in dial socket on `path`: every peer is 127.0.0.1, and `bastion` is its name;
/// others are refused the way the helper refuses them.
pub fn dial_socket(path: &Path) {
    let ln = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for conn in ln.incoming() {
            let Ok(conn) = conn else { break };
            std::thread::spawn(move || serve_dial(conn));
        }
    });
}

fn serve_dial(mut conn: UnixStream) {
    let mut line = String::new();
    if BufReader::new(&conn).read_line(&mut line).is_err() {
        return;
    }
    let req: serde_json::Value = serde_json::from_str(&line).unwrap();
    let host = req["host"].as_str().unwrap_or_default().to_owned();
    if host != "127.0.0.1" && host != "bastion" {
        let refusal =
            serde_json::json!({"error": format!("no peer named {host:?}"), "code": "no_such_peer"});
        let _ = writeln!(conn, "{refusal}");
        return;
    }
    let port = req["port"].as_u64().unwrap() as u16;
    let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)) else {
        let _ = writeln!(
            conn,
            r#"{{"error":"connection refused","code":"unreachable"}}"#
        );
        return;
    };
    let _ = writeln!(
        conn,
        r#"{{"ok":true,"dns_name":"bastion.tail1234.ts.net","addr":"127.0.0.1:{port}"}}"#
    );
    let (mut c_rd, mut c_wr) = (conn.try_clone().unwrap(), conn);
    let (mut t_rd, mut t_wr) = (tcp.try_clone().unwrap(), tcp);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_rd, &mut t_wr);
        let _ = t_wr.shutdown(std::net::Shutdown::Both);
    });
    let _ = std::io::copy(&mut t_rd, &mut c_wr);
    let _ = c_wr.shutdown(std::net::Shutdown::Both);
}
