//! `thurm pair`: the phone's pairing code made on the host itself, for a daemon that serves over
//! Tailscale. The same `thurm://pair?d=` link the Mac's Pair iPhone sheet makes, with this host
//! alone and only its Tailscale address: the phone connects there, admitted by its Tailscale
//! identity, so there are no host keys and no device key to add.

use std::process::ExitCode;

use base64::Engine;
use qrcode::{Color, QrCode};
use thurm_client::Client;
use thurm_config::{Config, PushConfig};
use thurm_proto::tailnet::{HttpsState, TailscaleState};
use thurm_proto::{Request, Response, TailscaleStatus};

pub fn run(c: &Client, name: Option<String>, link_only: bool, json: bool) -> super::R {
    let Response::TailscaleStatus(status) = c.request(Request::TailscaleStatus)? else {
        return Err("unexpected response".into());
    };
    let routes = routes(&status)?;
    let first = routes
        .wss
        .as_deref()
        .or(routes.plain.first().map(String::as_str))
        .unwrap_or_default();
    let host = host_of(&status, first);
    let name = name.unwrap_or_else(|| status.hostname.clone());
    let user = user_name().ok_or("could not tell this user's name")?;
    let push = Config::load()
        .map_err(|e| format!("could not read the config, for the push relay: {e}"))?
        .push;
    let payload = payload(&name, &host, &user, &routes, &push);
    let link = link(&payload);
    if json {
        let out = serde_json::json!({ "link": link, "payload": payload });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if link_only {
        println!("{link}");
    } else {
        print!("{}", render(&link)?);
        println!("Scan this with the Thurm app (+), or paste the link:");
        println!("{link}");
        let reach: Vec<&str> = routes
            .wss
            .iter()
            .chain(&routes.plain)
            .map(String::as_str)
            .collect();
        println!(
            "The phone connects to {} and needs the tailnet's VPN (Tailscale, or your headscale client), signed in as {}.",
            reach.join(", "),
            status
                .account
                .as_ref()
                .map_or("the account that signed this host in", |a| a
                    .login_name
                    .as_str())
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The ways the phone can reach this host: `wss` once HTTPS is ready, and the plain tailnet
/// endpoints (`ws://`), IP literals first.
#[derive(Default)]
struct Routes {
    wss: Option<String>,
    plain: Vec<String>,
}

/// The routes the phone connects by, or why there are none yet.
fn routes(s: &TailscaleStatus) -> Result<Routes, String> {
    match s.state {
        TailscaleState::Running => {}
        TailscaleState::Off | TailscaleState::Stopped => {
            return Err(
                "this daemon does not serve over Tailscale: run `thurm tailscale on` first".into(),
            );
        }
        TailscaleState::NeedsLogin => {
            return Err(
                "sign this daemon in to Tailscale first: `thurm tailscale status` shows the link"
                    .into(),
            );
        }
        _ => return Err("Tailscale is not running yet: see `thurm tailscale status`".into()),
    }
    let wss = match (&s.https.state, &s.url) {
        (HttpsState::Ready, Some(url)) => Some(url.clone()),
        _ => None,
    };
    let plain = plain_urls(s);
    if wss.is_none() && plain.is_empty() {
        return Err(
            "no address for the phone yet: HTTPS is not ready on the tailnet (`thurm tailscale status` says why), \
             and no plain endpoint is listening. Set `tailscale.plain_listen` (e.g. \":7297\") for a tailnet \
             without HTTPS, such as a headscale, or `tailscale.host_listen` to use this machine's own Tailscale"
                .into(),
        );
    }
    Ok(Routes { wss, plain })
}

/// The plain endpoints of the node and of this host's own Tailscale, each once, IPv4 literals
/// first, then IPv6, then names.
fn plain_urls(s: &TailscaleStatus) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for url in s.node_urls.iter().chain(&s.host_urls) {
        if url.starts_with("ws://") && !urls.contains(url) {
            urls.push(url.clone());
        }
    }
    // IPv4 literals, then IPv6, then names; a stable sort keeps each group in its order.
    urls.sort_by_key(|url| match ws_host(url).parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => 0,
        Ok(std::net::IpAddr::V6(_)) => 1,
        Err(_) => 2,
    });
    urls
}

/// The host of a `ws://` or `wss://` address, without its port or path (an IPv6 literal without
/// its brackets).
fn ws_host(url: &str) -> &str {
    let rest = url.trim_start_matches("wss://").trim_start_matches("ws://");
    if let Some(v6) = rest.strip_prefix('[') {
        return v6.split(']').next().unwrap_or_default();
    }
    rest.split(['/', ':']).next().unwrap_or_default()
}

/// The host's tailnet name: the iOS app shows it, and needs a host in every entry.
fn host_of(s: &TailscaleStatus, route: &str) -> String {
    match &s.dns_name {
        Some(dns) if !dns.is_empty() => dns.trim_end_matches('.').to_owned(),
        _ => ws_host(route).to_owned(),
    }
}

/// What the code carries (`PairingPayload` in the iOS app): this host's routes, and the push
/// relay when this host's config has one. `tailnet_plain` is only there with plain routes.
fn payload(
    name: &str,
    host: &str,
    user: &str,
    routes: &Routes,
    push: &PushConfig,
) -> serde_json::Value {
    let mut d = serde_json::json!({
        "v": 1,
        "name": name,
        "host": host,
        "user": user,
        "host_keys": [],
    });
    if let Some(url) = &routes.wss {
        d["tailnet"] = serde_json::json!({ "url": url });
    }
    if !routes.plain.is_empty() {
        d["tailnet_plain"] = routes.plain.clone().into();
    }
    if !push.relay.is_empty() {
        d["relay"] = push.relay.clone().into();
        if !push.code.is_empty() {
            d["code"] = push.code.clone().into();
        }
    }
    d
}

fn link(payload: &serde_json::Value) -> String {
    let json = serde_json::to_vec(payload).unwrap_or_default();
    format!(
        "thurm://pair?d={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    )
}

/// The link as a QR code of half blocks, two modules per line: black on white whatever the
/// terminal's colors, with the quiet zone a camera needs around it.
fn render(text: &str) -> Result<String, Box<dyn std::error::Error>> {
    const QUIET: usize = 4;
    let code = QrCode::new(text.as_bytes())?;
    let width = code.width();
    let colors = code.to_colors();
    let dark = |x: usize, y: usize| {
        x >= QUIET
            && y >= QUIET
            && x < width + QUIET
            && y < width + QUIET
            && colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
    };
    let size = width + 2 * QUIET;
    let mut out = String::new();
    for y in (0..size).step_by(2) {
        out.push_str("\x1b[30;107m");
        for x in 0..size {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str("\x1b[0m\n");
    }
    Ok(out)
}

fn user_name() -> Option<String> {
    if let Ok(u) = std::env::var("USER")
        && !u.is_empty()
    {
        return Some(u);
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::tailnet::TailscaleHttps;

    fn serving() -> TailscaleStatus {
        TailscaleStatus {
            state: TailscaleState::Running,
            hostname: "bastion".into(),
            dns_name: Some("bastion.tail1234.ts.net.".into()),
            url: Some("wss://bastion.tail1234.ts.net/v1/connect".into()),
            https: TailscaleHttps {
                state: HttpsState::Ready,
                error: None,
            },
            ..Default::default()
        }
    }

    #[test]
    fn pairs_only_once_serving_https_or_plain() {
        let s = serving();
        assert_eq!(
            routes(&s).unwrap().wss.as_deref(),
            Some("wss://bastion.tail1234.ts.net/v1/connect")
        );
        let mut provisioning = serving();
        provisioning.https.state = HttpsState::Provisioning;
        let err = routes(&provisioning).err().unwrap();
        assert!(err.contains("HTTPS"), "{err}");
        assert!(err.contains("tailscale.plain_listen"), "{err}");
        assert!(err.contains("tailscale.host_listen"), "{err}");
        let off = TailscaleStatus::default();
        assert!(routes(&off).err().unwrap().contains("thurm tailscale on"));
        let login = TailscaleStatus {
            state: TailscaleState::NeedsLogin,
            ..Default::default()
        };
        assert!(routes(&login).err().unwrap().contains("sign"));
    }

    #[test]
    fn plain_routes_pair_without_https() {
        let s = TailscaleStatus {
            https: TailscaleHttps {
                state: HttpsState::CertsOff,
                error: None,
            },
            url: None,
            dns_name: Some("bastion.hs.example.".into()),
            host_urls: vec![
                "ws://bastion.hs.example:7297/v1/connect".into(),
                "ws://100.64.0.7:7297/v1/connect".into(),
            ],
            node_urls: vec![
                "ws://100.64.0.9:7297/v1/connect".into(),
                "ws://100.64.0.7:7297/v1/connect".into(),
                "ws://thurm.hs.example:7297/v1/connect".into(),
            ],
            ..serving()
        };
        let r = routes(&s).unwrap();
        assert!(r.wss.is_none());
        assert_eq!(
            r.plain,
            [
                "ws://100.64.0.9:7297/v1/connect",
                "ws://100.64.0.7:7297/v1/connect",
                "ws://thurm.hs.example:7297/v1/connect",
                "ws://bastion.hs.example:7297/v1/connect",
            ]
        );
        let p = payload(
            "bastion",
            "bastion.hs.example",
            "me",
            &r,
            &PushConfig::default(),
        );
        assert!(p.get("tailnet").is_none());
        assert_eq!(p["tailnet_plain"][0], "ws://100.64.0.9:7297/v1/connect");
        assert_eq!(p["tailnet_plain"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn both_routes_are_carried() {
        let mut s = serving();
        s.host_urls = vec!["ws://100.64.0.7:7297/v1/connect".into()];
        let r = routes(&s).unwrap();
        let p = payload("bastion", "bastion", "me", &r, &PushConfig::default());
        assert_eq!(
            p["tailnet"]["url"],
            "wss://bastion.tail1234.ts.net/v1/connect"
        );
        assert_eq!(
            p["tailnet_plain"],
            serde_json::json!(["ws://100.64.0.7:7297/v1/connect"])
        );
    }

    #[test]
    fn payload_without_plain_routes_has_no_plain_key() {
        let r = routes(&serving()).unwrap();
        let p = payload("bastion", "bastion", "me", &r, &PushConfig::default());
        assert!(p.get("tailnet_plain").is_none());
    }

    #[test]
    fn plain_routes_put_ipv4_first_then_ipv6_then_names() {
        let s = TailscaleStatus {
            host_urls: vec![
                "ws://bastion.hs.example:7297/v1/connect".into(),
                "ws://[fd7a:115c:a1e0::1]:7297/v1/connect".into(),
                "ws://100.64.0.7:7297/v1/connect".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            plain_urls(&s),
            [
                "ws://100.64.0.7:7297/v1/connect",
                "ws://[fd7a:115c:a1e0::1]:7297/v1/connect",
                "ws://bastion.hs.example:7297/v1/connect",
            ]
        );
    }

    #[test]
    fn ws_host_is_the_bare_address() {
        assert_eq!(
            ws_host("ws://[fd7a:115c::1]:7297/v1/connect"),
            "fd7a:115c::1"
        );
        assert_eq!(ws_host("ws://100.64.0.7:7297/v1/connect"), "100.64.0.7");
        assert_eq!(
            ws_host("wss://bastion.tail1234.ts.net/v1/connect"),
            "bastion.tail1234.ts.net"
        );
    }

    #[test]
    fn host_is_the_tailnet_name() {
        let s = serving();
        assert_eq!(
            host_of(&s, s.url.as_deref().unwrap()),
            "bastion.tail1234.ts.net"
        );
        let no_dns = TailscaleStatus {
            dns_name: None,
            ..serving()
        };
        assert_eq!(
            host_of(&no_dns, "wss://bastion.tail1234.ts.net/v1/connect"),
            "bastion.tail1234.ts.net"
        );
        assert_eq!(
            host_of(&no_dns, "ws://100.64.0.7:7297/v1/connect"),
            "100.64.0.7"
        );
    }

    /// What the iOS app's `PairingPayload.parse` reads: v 1, a host, a user, and a tailnet
    /// address standing in for host keys.
    #[test]
    fn link_carries_the_tailnet_address() {
        let url = "wss://bastion.tail1234.ts.net/v1/connect";
        let routes = Routes {
            wss: Some(url.into()),
            plain: Vec::new(),
        };
        let p = payload(
            "bastion",
            "bastion.tail1234.ts.net",
            "me",
            &routes,
            &PushConfig::default(),
        );
        let l = link(&p);
        let b64 = l.strip_prefix("thurm://pair?d=").unwrap();
        let back: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(back["v"], 1);
        assert_eq!(back["name"], "bastion");
        assert_eq!(back["host"], "bastion.tail1234.ts.net");
        assert_eq!(back["user"], "me");
        assert_eq!(back["host_keys"], serde_json::json!([]));
        assert_eq!(back["tailnet"]["url"], url);
        assert!(back.get("relay").is_none() && back.get("code").is_none());
        assert!(!b64.contains(['+', '/', '=']), "base64url without padding");
    }

    #[test]
    fn link_carries_the_push_relay() {
        let push = PushConfig {
            relay: "https://push.example".into(),
            code: "abc".into(),
        };
        let routes = Routes {
            wss: Some("wss://bastion/v1/connect".into()),
            plain: Vec::new(),
        };
        let p = payload("bastion", "bastion", "me", &routes, &push);
        assert_eq!(p["relay"], "https://push.example");
        assert_eq!(p["code"], "abc");
        let no_code = PushConfig {
            code: String::new(),
            ..push
        };
        assert!(
            payload("b", "b", "me", &routes, &no_code)
                .get("code")
                .is_none()
        );
    }

    #[test]
    fn renders_a_square_code_with_a_quiet_zone() {
        let out = render("thurm://pair?d=eyJ2IjoxfQ").unwrap();
        let rows: Vec<&str> = out.lines().collect();
        let cells = |r: &str| -> Vec<char> {
            r.trim_start_matches("\x1b[30;107m")
                .trim_end_matches("\x1b[0m")
                .chars()
                .collect()
        };
        let size = cells(rows[0]).len();
        assert!(rows.iter().all(|r| cells(r).len() == size));
        assert_eq!(rows.len(), size.div_ceil(2));
        assert!(
            rows[..2].iter().all(|r| cells(r).iter().all(|&c| c == ' ')),
            "four blank modules on top"
        );
        assert!(
            rows.iter().all(|r| {
                let c = cells(r);
                c[..4].iter().chain(&c[size - 4..]).all(|&c| c == ' ')
            }),
            "four blank modules on each side"
        );
    }
}
