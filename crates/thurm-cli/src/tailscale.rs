use std::process::ExitCode;
use std::time::Duration;

use clap::Subcommand;
use thurm_client::Client;
use thurm_proto::tailnet::{HttpsState, TailscaleState};
use thurm_proto::{Principal, PrincipalKind, Request, Response, TailscaleStatus};

/// Starting the node or signing out reaches Tailscale's coordination server.
const TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Subcommand)]
pub enum TailscaleCmd {
    /// Show whether the daemon serves over Tailscale, its address, and the sign-in link while
    /// one is needed.
    Status,
    /// Serve over the tailnet as a device of its own. Runs until the daemon restarts (the
    /// config's [tailscale] decides then; `thurm set tailscale.enabled true` keeps it on). The
    /// first time, sign in with the link this prints, or enroll with --auth-key-file. Running
    /// it again changes the options.
    On {
        /// The device's name on the tailnet (one DNS label).
        #[arg(long)]
        hostname: Option<String>,
        /// Admit every member of the tailnet, not only the account that signed the device in.
        #[arg(long)]
        allow_tailnet_members: bool,
        /// ACL tag to enroll with (repeatable); needs --allow-tailnet-members, since a tagged
        /// device has no owner.
        #[arg(long = "advertise-tag", value_name = "TAG")]
        advertise_tags: Vec<String>,
        /// A file holding an auth key, to enroll without signing in in a browser.
        #[arg(long, value_name = "FILE")]
        auth_key_file: Option<std::path::PathBuf>,
    },
    /// Stop serving over the tailnet. Tailnet connections close while sessions keep running,
    /// and the device stays signed in: the next `on` needs no sign-in. Use `logout` to forget
    /// the device. Running it twice succeeds.
    Off,
    /// Ask Tailscale for a sign-in link for the device.
    Login,
    /// Sign the device out and forget it. A failed logout leaves it running, to retry.
    Logout,
    /// Rename the device without signing in again. NAME is one DNS label of 1 to 63 letters,
    /// digits or hyphens; the new DNS name may show a moment after this returns.
    Rename { name: String },
    /// The tailnet's devices, as the device sees them (it must be on).
    Peers,
    /// Connect stdin and stdout to PORT on a tailnet peer through the device, for ssh with the
    /// Tailscale app stopped: `ProxyCommand thurm tailscale nc %h %p`.
    Nc { host: String, port: u16 },
}

pub fn run(c: &Client, action: TailscaleCmd, json: bool) -> super::R {
    let request = match action {
        TailscaleCmd::Status => Request::TailscaleStatus,
        TailscaleCmd::On {
            hostname,
            allow_tailnet_members,
            advertise_tags,
            auth_key_file,
        } => {
            let auth_key = match auth_key_file {
                Some(path) => {
                    let key = std::fs::read_to_string(&path)
                        .map_err(|e| format!("reading tailscale auth key: {e}"))?;
                    let key = key.trim().to_owned();
                    if key.is_empty() {
                        return Err("tailscale auth key is empty".into());
                    }
                    Some(key)
                }
                None => None,
            };
            Request::TailscaleEnable {
                hostname,
                allow_tailnet_members,
                advertise_tags,
                auth_key,
            }
        }
        TailscaleCmd::Off => Request::TailscaleDisable,
        TailscaleCmd::Login => Request::TailscaleLogin,
        TailscaleCmd::Logout => Request::TailscaleLogout,
        TailscaleCmd::Rename { name } => Request::TailscaleRename { hostname: name },
        TailscaleCmd::Peers | TailscaleCmd::Nc { .. } => {
            unreachable!("run through the dial socket")
        }
    };
    let Response::TailscaleStatus(status) = c.request_timeout(request, Some(TIMEOUT))? else {
        return Err("unexpected response".into());
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        let browser = thurm_config::Config::load()
            .unwrap_or_default()
            .tailscale
            .web;
        print!("{}", describe(&status, browser));
    }
    Ok(ExitCode::SUCCESS)
}

pub fn whoami(c: &Client, json: bool) -> super::R {
    let Response::Whoami(p) = c.request(Request::Whoami)? else {
        return Err("unexpected response".into());
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&p)?);
    } else {
        print!("{}", describe_principal(&p));
    }
    Ok(ExitCode::SUCCESS)
}

/// `browser`: the config serves the browser client, so the page's address is printed too.
fn describe(s: &TailscaleStatus, browser: bool) -> String {
    let mut out = String::new();
    let state = match s.state {
        TailscaleState::Off => "not serving over Tailscale",
        TailscaleState::Starting => "starting",
        TailscaleState::NeedsLogin => "needs you to sign in to Tailscale",
        TailscaleState::NeedsMachineAuth => "waiting for a tailnet administrator to approve it",
        TailscaleState::Running => "serving over Tailscale",
        TailscaleState::Stopped => "stopped",
        TailscaleState::Error => "error",
    };
    out.push_str(&format!("{}: {state}\n", s.hostname));
    if let Some(url) = &s.auth_url {
        out.push_str(&format!("sign in: {url}\n"));
    } else if s.state == TailscaleState::NeedsLogin {
        out.push_str("no sign-in link yet; run `thurm tailscale status` again\n");
    }
    if let Some(name) = &s.dns_name {
        out.push_str(&format!("address: {name}\n"));
    }
    if let Some(url) = &s.url {
        out.push_str(&format!("connect: {url}\n"));
    }
    // Served once the node runs with HTTPS ready, which is when `url` is set.
    if browser
        && s.url.is_some()
        && let Some(name) = &s.dns_name
    {
        out.push_str(&format!("browser: https://{name}/\n"));
    }
    for url in &s.host_urls {
        out.push_str(&format!("connect (this host's Tailscale): {url}\n"));
    }
    if let Some(sock) = &s.serve_socket {
        out.push_str(&format!(
            "tailscale serve: tailscale serve --bg unix:{sock}\n"
        ));
    }
    if let Some(a) = &s.account {
        out.push_str(&format!("account: {}\n", a.login_name));
    }
    if let Some(t) = &s.tailnet_name {
        out.push_str(&format!("tailnet: {t}\n"));
    }
    if s.state != TailscaleState::Off {
        out.push_str(&format!(
            "access: {}\n",
            if s.allow_tailnet_members {
                "every tailnet member"
            } else {
                "only the account that signed it in"
            }
        ));
    }
    if !s.tags.is_empty() {
        out.push_str(&format!("tags: {}\n", s.tags.join(", ")));
    }
    let https = match s.https.state {
        HttpsState::Unavailable => None,
        HttpsState::MagicDnsOff => Some("off (MagicDNS is off)"),
        HttpsState::CertsOff => Some("off (HTTPS certificates are off)"),
        HttpsState::Provisioning => Some("getting a certificate"),
        HttpsState::Ready => Some("ready"),
    };
    if let Some(h) = https {
        out.push_str(&format!("https: {h}\n"));
    }
    if let Some(e) = s.https.error.as_ref().or(s.error.as_ref()) {
        out.push_str(&format!("note: {e}\n"));
    }
    out
}

fn describe_principal(p: &Principal) -> String {
    match p.kind {
        PrincipalKind::Peer => format!("{} on this machine\n", p.display),
        PrincipalKind::Tailscale => {
            let mut out = format!("{} over the tailnet", p.display);
            if let Some(node) = &p.node {
                out.push_str(&format!(" from {node}"));
            }
            if !p.tags.is_empty() {
                out.push_str(&format!(" (tagged {})", p.tags.join(", ")));
            }
            out.push('\n');
            out
        }
    }
}

/// `thurm tailscale peers`.
pub fn peers(json: bool) -> super::R {
    let socket = thurm_config::tailnet_dial_socket_path();
    let peers = thurm_remote::tailnet::peers(&socket, thurm_remote::tailnet::CONNECT_TIMEOUT)
        .map_err(dial_hint)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&peers)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut peers = peers;
    peers.sort_by(|a, b| a.dns_name.cmp(&b.dns_name));
    println!("{:<44} {:<8} ADDRESSES", "NAME", "ONLINE");
    for p in peers {
        println!(
            "{:<44} {:<8} {}",
            p.dns_name,
            if p.online { "yes" } else { "no" },
            p.addrs.join(", ")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `thurm tailscale nc HOST PORT`: stdin and stdout to the peer until either side ends.
pub fn nc(host: &str, port: u16) -> super::R {
    use std::io::{Read, Write};
    let socket = thurm_config::tailnet_dial_socket_path();
    let (stream, _) =
        thurm_remote::tailnet::dial(&socket, host, port, thurm_remote::tailnet::CONNECT_TIMEOUT)
            .map_err(dial_hint)?;
    let mut up = stream.try_clone()?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 32 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || up.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut down = stream;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = down.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
    Ok(ExitCode::SUCCESS)
}

fn dial_hint(e: thurm_remote::tailnet::DialError) -> String {
    match e {
        thurm_remote::tailnet::DialError::NoSocket(_) => format!(
            "{e}: turn it on with `thurm tailscale on` (a daemon that was upgraded in place \
             needs `thurm tailscale off` and `on` once)"
        ),
        e => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_a_sign_in() {
        let s = TailscaleStatus {
            state: TailscaleState::NeedsLogin,
            hostname: "thurm".into(),
            auth_url: Some("https://login.tailscale.com/a/abc".into()),
            ..Default::default()
        };
        let text = describe(&s, false);
        assert!(text.contains("needs you to sign in"), "{text}");
        assert!(text.contains("https://login.tailscale.com/a/abc"), "{text}");
    }

    #[test]
    fn prints_the_browser_page_when_it_is_served() {
        let s = TailscaleStatus {
            state: TailscaleState::Running,
            hostname: "thurm".into(),
            dns_name: Some("thurm.tail1234.ts.net".into()),
            url: Some("wss://thurm.tail1234.ts.net/v1/connect".into()),
            ..Default::default()
        };
        assert!(describe(&s, true).contains("browser: https://thurm.tail1234.ts.net/\n"));
        assert!(!describe(&s, false).contains("browser:"));
        // Not before HTTPS is ready: the node doesn't serve the page yet.
        let waiting = TailscaleStatus { url: None, ..s };
        assert!(!describe(&waiting, true).contains("browser:"));
    }

    #[test]
    fn describes_running() {
        let s = TailscaleStatus {
            state: TailscaleState::Running,
            hostname: "thurm".into(),
            dns_name: Some("thurm.tail1234.ts.net".into()),
            url: Some("wss://thurm.tail1234.ts.net/v1/connect".into()),
            https: thurm_proto::tailnet::TailscaleHttps {
                state: HttpsState::Ready,
                error: None,
            },
            ..Default::default()
        };
        let text = describe(&s, false);
        assert!(text.contains("serving over Tailscale"), "{text}");
        assert!(text.contains("only the account"), "{text}");
        assert!(text.contains("https: ready"), "{text}");
    }
}
