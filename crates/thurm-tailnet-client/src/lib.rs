//! Connects to a daemon served over Tailscale (`thurm-tailnet`): a WebSocket at
//! `wss://<device>.<tailnet>.ts.net/v1/connect` (or `ws://` on a host's own tailnet address)
//! whose binary messages carry the daemon's byte stream unchanged.
//!
//! The device reaches the tailnet through its own Tailscale (the Tailscale app on iOS), or the
//! caller dials the daemon's device itself and passes the connected socket
//! ([`connect_fd`], for an app embedding Tailscale); this only speaks WebSocket over TLS. The daemon admits the caller by its Tailscale identity, so
//! there is no key or password here. The relay ends in a Unix socket pair, so the connection is
//! an ordinary [`Client`] (the same trick as `thurm_ssh::Session::open_socket`).

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use thurm_client::{Client, ClientError};
use thurm_proto::Event;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::{self, Message};

/// How long reaching the daemon may take: TCP, TLS and the upgrade. Short, so a device whose
/// Tailscale is off falls back to ssh quickly (a node that is off doesn't refuse; it's silent).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// A ping once the daemon has been quiet this long. Never while it talks: the gateway answers a
/// ping only once its own writes let go of the socket, within a few seconds.
const PING_EVERY: Duration = Duration::from_secs(15);
/// Nothing heard for this long (not even a pong): the connection is gone.
const SILENT_LIMIT: Duration = Duration::from_secs(45);

#[derive(Debug)]
pub enum ConnectError {
    /// Not a ws:// or wss:// URL.
    BadUrl(String),
    /// No connection: Tailscale is off on this device, or the daemon's device is.
    Unreachable(String),
    /// The daemon's device refused this device's Tailscale account (HTTP 403).
    Forbidden(String),
    /// The daemon's device answered, but its daemon doesn't (HTTP 503).
    DaemonUnavailable,
    /// Something answered that isn't a Thurm daemon, or the TLS handshake failed.
    Protocol(String),
    /// The daemon refused the Hello.
    Daemon(ClientError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::BadUrl(e) => write!(f, "not a Thurm tailnet address: {e}"),
            ConnectError::Unreachable(e) => write!(
                f,
                "can't reach the host over Tailscale ({e}): is Tailscale connected on this \
                 device, and is Serve over Tailscale on on the host?"
            ),
            ConnectError::Forbidden(e) => write!(
                f,
                "the host doesn't let this device's Tailscale account in: {e}"
            ),
            ConnectError::DaemonUnavailable => {
                write!(
                    f,
                    "the host answered over Tailscale, but Thurm's daemon isn't running there"
                )
            }
            ConnectError::Protocol(e) => write!(f, "{e}"),
            ConnectError::Daemon(e) => write!(f, "the daemon refused the connection: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// The relay's runtime: dropping it ends the relay, and with it the connection.
pub struct Session {
    rt: Option<Runtime>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Not waiting: the relay tasks end as their sockets close.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

pub struct Remote {
    pub client: Arc<Client>,
    pub session: Session,
}

/// The relay's timing; tests shorten it.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// For everything up to the daemon's answer to Hello.
    connect: Duration,
    ping_every: Duration,
    silent: Duration,
}

const LIMITS: Limits = Limits {
    connect: CONNECT_TIMEOUT,
    ping_every: PING_EVERY,
    silent: SILENT_LIMIT,
};

/// Connects to the daemon at `url` (`wss://` only) and says Hello (with `capabilities`, e.g.
/// `CAP_MOBILE`), within [`CONNECT_TIMEOUT`] in all.
pub fn connect(
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    check_wss_url(url)?;
    connect_with(
        url,
        client_name,
        capabilities,
        LIMITS,
        on_event,
        on_disconnect,
    )
}

/// Connects to a daemon's plain tailnet listener over the device's system Tailscale VPN.
/// Strictly requires `ws://`, a Tailscale IP literal host (`100.64.0.0/10` IPv4 CGNAT or
/// `fd7a:115c:a1e0::/48` IPv6 ULA), AND an active system Tailscale tunnel interface (`utun*` on
/// Apple / `tailscale*` on Linux). The TCP socket is bound directly to that tunnel interface
/// (`IP_BOUND_IF` / `IPV6_BOUND_IF` / `SO_BINDTODEVICE`) and its Tailscale source address,
/// failing closed whenever the VPN is down or ambiguous so cleartext traffic never reaches a
/// physical or carrier CGNAT route.
pub fn connect_plain(
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let deadline = std::time::Instant::now() + LIMITS.connect;
    let opened = open_plain_with(url, LIMITS, None)?;
    hello(
        opened,
        deadline,
        client_name,
        capabilities,
        on_event,
        on_disconnect,
    )
}

/// Like [`connect`], over `fd`: a stream socket already connected to the daemon's device (say,
/// by an embedded Tailscale's dial), on which this does TLS (for `url`'s host) and the upgrade.
/// The socket is closed when the connection ends or fails.
pub fn connect_fd(
    fd: OwnedFd,
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let deadline = std::time::Instant::now() + LIMITS.connect;
    let opened = open_fd_with(fd, url, LIMITS)?;
    hello(
        opened,
        deadline,
        client_name,
        capabilities,
        on_event,
        on_disconnect,
    )
}

/// Like [`connect_fd`], but `url` may be `ws://`: the caller vouches that `fd` reaches its host
/// through WireGuard (see [`open_tailnet_fd`]).
pub fn connect_tailnet_fd(
    fd: OwnedFd,
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let deadline = std::time::Instant::now() + LIMITS.connect;
    let opened = open_tailnet_fd(fd, url, LIMITS.connect)?;
    hello(
        opened,
        deadline,
        client_name,
        capabilities,
        on_event,
        on_disconnect,
    )
}

fn connect_with(
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    limits: Limits,
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let deadline = std::time::Instant::now() + limits.connect;
    let opened = open_with(url, limits)?;
    hello(
        opened,
        deadline,
        client_name,
        capabilities,
        on_event,
        on_disconnect,
    )
}

/// Says Hello over an opened relay, by `deadline`.
fn hello(
    (stream, session): (UnixStream, Session),
    deadline: std::time::Instant,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    // A device that upgrades but whose daemon never answers must fail as quickly as one that
    // doesn't answer at all.
    let left = deadline
        .saturating_duration_since(std::time::Instant::now())
        .max(Duration::from_millis(500));
    let client = Client::from_stream_timeout(
        stream,
        client_name,
        false,
        capabilities,
        Some(left),
        on_event,
        on_disconnect,
    )
    .map_err(|e| match e {
        ClientError::Timeout => ConnectError::DaemonUnavailable,
        e => ConnectError::Daemon(e),
    })?;
    Ok(Remote { client, session })
}

/// Opens the `wss://` WebSocket at `url` and relays it to one end of a socket pair, returned with
/// the session that keeps the relay running.
pub fn open(url: &str, timeout: Duration) -> Result<(UnixStream, Session), ConnectError> {
    check_wss_url(url)?;
    open_with(
        url,
        Limits {
            connect: timeout,
            ..LIMITS
        },
    )
}

/// Opens a plain `ws://` WebSocket to a Tailscale IP literal (`100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48`) bound directly to the device's active Tailscale VPN tunnel interface,
/// and relays it to one end of a socket pair. Fails closed when the system Tailscale VPN is down
/// or its route is ambiguous.
pub fn open_plain(url: &str, timeout: Duration) -> Result<(UnixStream, Session), ConnectError> {
    open_plain_with(
        url,
        Limits {
            connect: timeout,
            ..LIMITS
        },
        None,
    )
}

/// Like [`open`], over `fd` (see [`connect_fd`]).
pub fn open_fd(
    fd: OwnedFd,
    url: &str,
    timeout: Duration,
) -> Result<(UnixStream, Session), ConnectError> {
    open_fd_with(
        fd,
        url,
        Limits {
            connect: timeout,
            ..LIMITS
        },
    )
}

fn check_url(url: &str) -> Result<(), ConnectError> {
    if url.starts_with("wss://") || url.starts_with("ws://") {
        Ok(())
    } else {
        Err(ConnectError::BadUrl(url.to_owned()))
    }
}

fn check_wss_url(url: &str) -> Result<(), ConnectError> {
    if url.starts_with("wss://") {
        Ok(())
    } else {
        Err(ConnectError::BadUrl(format!(
            "{url}: expected a wss:// address"
        )))
    }
}

/// Extracts the bare host and port from a `ws://` URL, rejecting userinfo (`@`).
fn extract_ws_host_port(url: &str) -> Option<(&str, u16)> {
    let rest = url.strip_prefix("ws://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(after_open) = authority.strip_prefix('[') {
        let (host, tail) = after_open.split_once(']')?;
        let port = if tail.is_empty() {
            80
        } else {
            let p = tail.strip_prefix(':')?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let parsed: u16 = p.parse().ok()?;
            if parsed == 0 {
                return None;
            }
            parsed
        };
        (!host.is_empty()).then_some((host, port))
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let parsed: u16 = p.parse().ok()?;
                if parsed == 0 {
                    return None;
                }
                (h, parsed)
            }
            None => (authority, 80),
        };
        (!host.is_empty()).then_some((host, port))
    }
}

fn is_tailnet_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

fn is_tailnet_v6(ip: std::net::Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

fn is_tailnet_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_tailnet_v4(v4),
        std::net::IpAddr::V6(v6) => is_tailnet_v6(v6),
    }
}

fn check_plain_tailnet_url(url: &str) -> Result<std::net::SocketAddr, ConnectError> {
    let (host, port) = extract_ws_host_port(url).ok_or_else(|| {
        ConnectError::BadUrl(format!(
            "{url}: plain tailnet connect requires a ws:// URL with a Tailscale IP literal"
        ))
    })?;
    let ip: std::net::IpAddr = host.parse().map_err(|_| {
        ConnectError::BadUrl(format!(
            "{url}: plain tailnet connect requires a Tailscale IP literal host"
        ))
    })?;
    if !is_tailnet_ip(ip) {
        return Err(ConnectError::BadUrl(format!(
            "{url}: {ip} is not a Tailscale IP address"
        )));
    }
    Ok(std::net::SocketAddr::new(ip, port))
}

/// An active (`IFF_UP | IFF_RUNNING`, non-loopback) OS network interface address.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InterfaceAddr {
    name: String,
    index: u32,
    addr: std::net::IpAddr,
    point_to_point: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SelectedTailnetIface {
    name: String,
    index: u32,
    local_ip: std::net::IpAddr,
}

fn is_tailnet_tunnel_iface(name: &str, point_to_point: bool) -> bool {
    #[cfg(target_vendor = "apple")]
    {
        name.starts_with("utun") && point_to_point
    }
    #[cfg(target_os = "linux")]
    {
        let _ = point_to_point;
        name.starts_with("tailscale")
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        let _ = (name, point_to_point);
        false
    }
}

/// Selects the unique active Tailscale VPN tunnel interface that owns a local Tailscale address
/// in `target`'s family. Never selects physical or cellular interfaces (e.g. `en0`, `pdp_ip0`),
/// prefers interfaces carrying Tailscale's globally unique ULA (`fd7a:115c:a1e0::/48`), and fails
/// closed if no tunnel is active or multiple tunnels ambiguously claim `100.64.0.0/10`.
fn select_tailnet_interface(
    target: std::net::IpAddr,
    ifaces: &[InterfaceAddr],
) -> Result<SelectedTailnetIface, ConnectError> {
    struct Candidate {
        name: String,
        index: u32,
        v4: Option<std::net::Ipv4Addr>,
        v6: Option<std::net::Ipv6Addr>,
    }
    let mut by_iface: Vec<Candidate> = Vec::new();
    for entry in ifaces {
        if entry.index == 0 || !is_tailnet_tunnel_iface(&entry.name, entry.point_to_point) {
            continue;
        }
        let cand = match by_iface
            .iter_mut()
            .find(|c| c.index == entry.index && c.name == entry.name)
        {
            Some(c) => c,
            None => {
                by_iface.push(Candidate {
                    name: entry.name.clone(),
                    index: entry.index,
                    v4: None,
                    v6: None,
                });
                by_iface.last_mut().expect("just pushed")
            }
        };
        match entry.addr {
            std::net::IpAddr::V4(v4) if is_tailnet_v4(v4) && cand.v4.is_none() => {
                cand.v4 = Some(v4);
            }
            std::net::IpAddr::V6(v6) if is_tailnet_v6(v6) && cand.v6.is_none() => {
                cand.v6 = Some(v6);
            }
            _ => {}
        }
    }

    let matching: Vec<&Candidate> = by_iface
        .iter()
        .filter(|c| match target {
            std::net::IpAddr::V4(_) => c.v4.is_some(),
            std::net::IpAddr::V6(_) => c.v6.is_some(),
        })
        .collect();

    if matching.is_empty() {
        return Err(ConnectError::Unreachable(
            "no active Tailscale VPN tunnel interface for plain tailnet connection; \
             use wss:// or embedded Tailscale"
                .into(),
        ));
    }

    let chosen = match target {
        std::net::IpAddr::V6(_) => {
            if matching.len() != 1 {
                return Err(ConnectError::Unreachable(
                    "multiple active Tailscale IPv6 tunnel interfaces; refusing ambiguous plain route"
                        .into(),
                ));
            }
            matching[0]
        }
        std::net::IpAddr::V4(_) => {
            // Tailscale's fd7a:115c:a1e0::/48 ULA uniquely identifies a Tailscale tunnel over
            // generic CGNAT VPNs. On Apple, every VPN creates a `utun*` interface, so require the
            // Tailscale ULA to prove the interface belongs to Tailscale rather than another VPN
            // using 100.64.0.0/10. On Linux, the interface name (`tailscale*`) already identifies
            // Tailscale's TUN interface.
            let ula_confirmed: Vec<&Candidate> =
                matching.iter().copied().filter(|c| c.v6.is_some()).collect();
            if ula_confirmed.len() == 1 {
                ula_confirmed[0]
            } else if ula_confirmed.len() > 1 {
                return Err(ConnectError::Unreachable(
                    "multiple active Tailscale tunnel interfaces; refusing ambiguous plain route"
                        .into(),
                ));
            } else {
                #[cfg(target_os = "linux")]
                {
                    if matching.len() == 1 {
                        matching[0]
                    } else {
                        return Err(ConnectError::Unreachable(
                            "multiple tailscale* interfaces hold 100.64.0.0/10 addresses; \
                             refusing ambiguous plain route"
                                .into(),
                        ));
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    return Err(ConnectError::Unreachable(
                        "no utun interface confirmed as Tailscale (missing fd7a:115c:a1e0::/48 ULA); \
                         refusing unverified 100.64.0.0/10 plain route"
                            .into(),
                    ));
                }
            }
        }
    };

    let local_ip = match target {
        std::net::IpAddr::V4(_) => std::net::IpAddr::V4(chosen.v4.expect("filtered")),
        std::net::IpAddr::V6(_) => std::net::IpAddr::V6(chosen.v6.expect("filtered")),
    };
    Ok(SelectedTailnetIface {
        name: chosen.name.clone(),
        index: chosen.index,
        local_ip,
    })
}

#[cfg(unix)]
fn list_active_interfaces() -> std::io::Result<Vec<InterfaceAddr>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    struct Guard(*mut libc::ifaddrs);
    impl Drop for Guard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { libc::freeifaddrs(self.0) };
            }
        }
    }
    let _guard = Guard(head);
    let required = (libc::IFF_UP | libc::IFF_RUNNING) as u32;
    let loopback = libc::IFF_LOOPBACK as u32;
    let ptp = libc::IFF_POINTOPOINT as u32;
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        let flags = ifa.ifa_flags;
        if (flags & required) != required || (flags & loopback) != 0 {
            continue;
        }
        if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() {
            continue;
        }
        let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
        let addr = match family {
            libc::AF_INET => {
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                std::net::IpAddr::V4(ip)
            }
            libc::AF_INET6 => {
                let sin6 = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                std::net::IpAddr::V6(ip)
            }
            _ => continue,
        };
        let index = unsafe { libc::if_nametoindex(ifa.ifa_name) };
        if index == 0 {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }
            .to_string_lossy()
            .into_owned();
        out.push(InterfaceAddr {
            name,
            index,
            addr,
            point_to_point: (flags & ptp) != 0,
        });
    }
    Ok(out)
}

#[cfg(not(unix))]
fn list_active_interfaces() -> std::io::Result<Vec<InterfaceAddr>> {
    Ok(Vec::new())
}

/// Hard-binds `socket` to the verified Tailscale tunnel interface and its local Tailscale IP so
/// the kernel refuses to route packets over physical or carrier CGNAT interfaces even if the VPN
/// disconnects mid-dial.
fn bind_to_tailnet_interface(
    socket: &tokio::net::TcpSocket,
    iface: &SelectedTailnetIface,
) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let raw = socket.as_raw_fd();
        let idx: libc::c_uint = iface.index;
        let (level, opt) = match iface.local_ip {
            std::net::IpAddr::V4(_) => (libc::IPPROTO_IP, libc::IP_BOUND_IF),
            std::net::IpAddr::V6(_) => (libc::IPPROTO_IPV6, libc::IPV6_BOUND_IF),
        };
        let rc = unsafe {
            libc::setsockopt(
                raw,
                level,
                opt,
                (&idx as *const libc::c_uint).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let raw = socket.as_raw_fd();
        let cname = std::ffi::CString::new(iface.name.as_str()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid interface name")
        })?;
        let rc = unsafe {
            libc::setsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                cname.as_ptr().cast::<libc::c_void>(),
                cname.as_bytes_with_nul().len() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        let _ = (socket, iface);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "interface-bound plain tailnet sockets are not supported on this platform",
        ));
    }
    socket.bind(std::net::SocketAddr::new(iface.local_ip, 0))
}

fn open_plain_with(
    url: &str,
    limits: Limits,
    override_ifaces: Option<&[InterfaceAddr]>,
) -> Result<(UnixStream, Session), ConnectError> {
    let target_addr = check_plain_tailnet_url(url)?;
    let owned_ifaces;
    let ifaces = match override_ifaces {
        Some(list) => list,
        None => {
            owned_ifaces = list_active_interfaces()
                .map_err(|e| ConnectError::Unreachable(format!("inspecting interfaces: {e}")))?;
            &owned_ifaces
        }
    };
    let selected = select_tailnet_interface(target_addr.ip(), ifaces)?;
    let session = runtime()?;
    let rt = session.rt.as_ref().expect("just made");
    let deadline = std::time::Instant::now() + limits.connect;
    let ws = rt.block_on(async {
        let socket = match target_addr {
            std::net::SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            std::net::SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        }
        .map_err(|e| ConnectError::Unreachable(format!("creating socket: {e}")))?;
        bind_to_tailnet_interface(&socket, &selected).map_err(|e| {
            ConnectError::Unreachable(format!(
                "binding plain tailnet socket to {}: {e}",
                selected.name
            ))
        })?;
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let tcp = match tokio::time::timeout(left, socket.connect(target_addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(ConnectError::Unreachable(e.to_string())),
            Err(_) => {
                return Err(ConnectError::Unreachable(format!(
                    "no answer within {} seconds",
                    limits.connect.as_secs()
                )));
            }
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        handshake_within(left, tokio_tungstenite::client_async(url, tcp)).await
    })?;
    start(ws, session, limits)
}

fn runtime() -> Result<Session, ConnectError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thurm-tailnet")
        .enable_all()
        .build()
        .map_err(|e| ConnectError::Protocol(format!("starting the relay: {e}")))?;
    Ok(Session { rt: Some(rt) })
}

/// The handshake's result, with `timeout` as the limit.
async fn handshake_within<S>(
    timeout: Duration,
    handshake: impl std::future::Future<
        Output = Result<
            (
                tokio_tungstenite::WebSocketStream<S>,
                tungstenite::handshake::client::Response,
            ),
            tungstenite::Error,
        >,
    >,
) -> Result<tokio_tungstenite::WebSocketStream<S>, ConnectError> {
    match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(classify(e)),
        Err(_) => Err(ConnectError::Unreachable(format!(
            "no answer within {} seconds",
            timeout.as_secs()
        ))),
    }
}

fn open_with(url: &str, limits: Limits) -> Result<(UnixStream, Session), ConnectError> {
    check_url(url)?;
    let session = runtime()?;
    let rt = session.rt.as_ref().expect("just made");
    let ws = rt.block_on(async {
        let connector = tokio_tungstenite::Connector::Rustls(tls_config()?);
        handshake_within(
            limits.connect,
            tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector)),
        )
        .await
    })?;
    start(ws, session, limits)
}

/// Like [`open_fd`], but `ws://` is accepted too: the caller vouches that `fd` reaches `url`'s
/// host through WireGuard (Thurm's embedded node dialed a peer from its netmap), so the tailnet
/// ties the connection to the host where TLS otherwise would. `wss://` still does TLS.
pub fn open_tailnet_fd(
    fd: OwnedFd,
    url: &str,
    timeout: Duration,
) -> Result<(UnixStream, Session), ConnectError> {
    check_url(url)?;
    open_fd_tls(
        fd,
        url,
        Limits {
            connect: timeout,
            ..LIMITS
        },
        tls_config()?,
        true,
    )
}

fn open_fd_with(
    fd: OwnedFd,
    url: &str,
    limits: Limits,
) -> Result<(UnixStream, Session), ConnectError> {
    open_fd_tls(fd, url, limits, tls_config()?, false)
}

/// Over a socket the caller dialed, TLS is what ties the connection to `url`'s host: only
/// `wss://` is accepted, unless `plain_ok` says the dial itself did ([`open_tailnet_fd`]).
fn open_fd_tls(
    fd: OwnedFd,
    url: &str,
    limits: Limits,
    tls: Arc<rustls::ClientConfig>,
    plain_ok: bool,
) -> Result<(UnixStream, Session), ConnectError> {
    if !url.starts_with("wss://") && !plain_ok {
        return Err(ConnectError::BadUrl(format!(
            "{url}: a dialed socket needs a wss:// address"
        )));
    }
    let socket = UnixStream::from(fd);
    socket
        .set_nonblocking(true)
        .map_err(|e| ConnectError::Protocol(format!("the dialed socket: {e}")))?;
    let session = runtime()?;
    let rt = session.rt.as_ref().expect("just made");
    let ws = rt.block_on(async {
        let socket = tokio::net::UnixStream::from_std(socket)
            .map_err(|e| ConnectError::Protocol(format!("the dialed socket: {e}")))?;
        let connector = tokio_tungstenite::Connector::Rustls(tls);
        handshake_within(
            limits.connect,
            tokio_tungstenite::client_async_tls_with_config(url, socket, None, Some(connector)),
        )
        .await
    })?;
    start(ws, session, limits)
}

/// Relays `ws` to one end of a new socket pair, returned.
fn start<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    session: Session,
    limits: Limits,
) -> Result<(UnixStream, Session), ConnectError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let rt = session.rt.as_ref().expect("made with it");
    let (ours, theirs) =
        UnixStream::pair().map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?;
    theirs
        .set_nonblocking(true)
        .map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?;
    let theirs = {
        let _guard = rt.enter();
        tokio::net::UnixStream::from_std(theirs)
            .map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?
    };
    rt.spawn(relay(ws, theirs, limits));
    Ok((ours, session))
}

/// Publicly trusted roots: ts.net certificates are Let's Encrypt's.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, ConnectError> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| ConnectError::Protocol(format!("TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn classify(e: tungstenite::Error) -> ConnectError {
    match e {
        tungstenite::Error::Http(resp) => {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_owned())
                .unwrap_or_default();
            match resp.status().as_u16() {
                403 => ConnectError::Forbidden(if body.is_empty() {
                    "forbidden".into()
                } else {
                    body
                }),
                503 => ConnectError::DaemonUnavailable,
                code => ConnectError::Protocol(format!(
                    "the address answered HTTP {code}, not a Thurm daemon"
                )),
            }
        }
        // A certificate the address can't vouch for is not an unreachable address.
        tungstenite::Error::Io(e)
            if e.get_ref()
                .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some()) =>
        {
            ConnectError::Protocol(format!("TLS: {e}"))
        }
        tungstenite::Error::Io(e) => ConnectError::Unreachable(e.to_string()),
        tungstenite::Error::Url(e) => ConnectError::BadUrl(e.to_string()),
        tungstenite::Error::Tls(e) => ConnectError::Protocol(format!("TLS: {e}")),
        e => ConnectError::Protocol(e.to_string()),
    }
}

/// Sends `msg`, giving up after `limit`: a peer that stopped reading must not hold the relay
/// (and its silence check) forever.
async fn send_within<S: AsyncRead + AsyncWrite + Unpin>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    msg: Message,
    limit: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(limit, sink.send(msg)).await,
        Ok(Ok(()))
    )
}

/// What the relay knows about the daemon's side of the connection.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Heard {
    /// Last heard at this instant.
    At(tokio::time::Instant),
    /// Its bytes are being written to the local app now. Nothing is heard meanwhile, and that
    /// wait is the app's, not the daemon's silence.
    Writing,
}

/// The silence left before the connection is closed: a send may use no more than this, so a
/// stalled send can't stretch the limit.
fn silence_left(heard: Heard, now: tokio::time::Instant, silent: Duration) -> Duration {
    match heard {
        Heard::At(at) => silent.saturating_sub(now.saturating_duration_since(at)),
        Heard::Writing => silent,
    }
}

/// What the relay does about a quiet daemon.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Quiet {
    /// Look again in this long.
    Wait(Duration),
    /// Quiet for `ping_every`, and not pinged since it was last heard.
    Ping,
    /// Silent for the whole limit: gone.
    Close,
}

/// What to do at `now`, given when the daemon was last heard and when it was last pinged.
fn quiet(
    heard: Heard,
    pinged: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
    ping_every: Duration,
    silent: Duration,
) -> Quiet {
    let Heard::At(at) = heard else {
        return Quiet::Wait(ping_every);
    };
    let silence = now.saturating_duration_since(at);
    if silence >= silent {
        return Quiet::Close;
    }
    if silence < ping_every {
        return Quiet::Wait(ping_every - silence);
    }
    // Pinged since it was last heard: wait for the pong, or for the limit.
    if pinged.is_some_and(|p| p > at) {
        return Quiet::Wait(silent - silence);
    }
    Quiet::Ping
}

/// Whether a write whose last bytes went out at `last` has stalled: no bytes for `idle`.
fn stalled(last: tokio::time::Instant, now: tokio::time::Instant, idle: Duration) -> bool {
    now.saturating_duration_since(last) >= idle
}

/// Writes `data` to the local app, giving up once it takes no bytes for `idle`. An app that
/// keeps reading is not stalled, however long the whole write takes.
async fn write_progressing<W: AsyncWrite + Unpin>(
    wr: &mut W,
    mut data: &[u8],
    idle: Duration,
) -> bool {
    let mut last = tokio::time::Instant::now();
    while !data.is_empty() {
        tokio::select! {
            written = wr.write(data) => match written {
                Ok(n) if n > 0 => {
                    data = &data[n..];
                    last = tokio::time::Instant::now();
                }
                _ => return false,
            },
            _ = tokio::time::sleep_until(last + idle) => {
                if stalled(last, tokio::time::Instant::now(), idle) {
                    return false;
                }
            }
        }
    }
    true
}

/// Copies the daemon's bytes both ways until either side ends, pinging while it's quiet.
async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    local: tokio::net::UnixStream,
    limits: Limits,
) {
    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = local.into_split();
    // The daemon's side, as the down side keeps it current.
    let (heard_tx, heard_rx) = tokio::sync::watch::channel(Heard::At(tokio::time::Instant::now()));
    let budget = || {
        silence_left(
            *heard_rx.borrow(),
            tokio::time::Instant::now(),
            limits.silent,
        )
    };

    let down = async {
        while let Some(msg) = stream.next().await {
            let _ = heard_tx.send(Heard::At(tokio::time::Instant::now()));
            match msg {
                Ok(Message::Binary(b)) => {
                    let _ = heard_tx.send(Heard::Writing);
                    let wrote = write_progressing(&mut wr, &b, limits.silent).await;
                    let _ = heard_tx.send(Heard::At(tokio::time::Instant::now()));
                    if !wrote {
                        log::info!("tailnet: the local app takes no bytes; closing");
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                // Pongs answer our pings; the library answers the daemon's pings itself.
                Ok(_) => {}
            }
        }
    };
    let up = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut pinged = None;
        loop {
            let heard = *heard_rx.borrow();
            let state = quiet(
                heard,
                pinged,
                tokio::time::Instant::now(),
                limits.ping_every,
                limits.silent,
            );
            let check = tokio::time::sleep(match state {
                Quiet::Wait(d) => d,
                Quiet::Ping | Quiet::Close => Duration::ZERO,
            });
            tokio::select! {
                n = rd.read(&mut buf) => match n {
                    Ok(0) | Err(_) => {
                        send_within(&mut sink, Message::Close(None), Duration::from_secs(2)).await;
                        break;
                    }
                    Ok(n) => {
                        let msg = Message::Binary(buf[..n].to_vec().into());
                        if !send_within(&mut sink, msg, budget()).await {
                            log::info!("tailnet: the daemon stopped reading; closing");
                            break;
                        }
                    }
                },
                _ = check => match state {
                    Quiet::Wait(_) => {}
                    Quiet::Close => {
                        log::info!("tailnet: no answer for {}s; closing", limits.silent.as_secs());
                        break;
                    }
                    Quiet::Ping => {
                        pinged = Some(tokio::time::Instant::now());
                        if !send_within(&mut sink, Message::Ping(Vec::new().into()), budget()).await {
                            break;
                        }
                    }
                },
            }
        }
    };
    // Either side ending ends both: the socket pair closes and the client sees the disconnect.
    tokio::select! {
        _ = down => {}
        _ = up => {}
    }
}

#[cfg(test)]
mod tests;
