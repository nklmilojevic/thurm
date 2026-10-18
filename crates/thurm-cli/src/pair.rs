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
    let url = tailnet_url(&status)?;
    let host = host_of(&status, url);
    let name = name.unwrap_or_else(|| status.hostname.clone());
    let user = user_name().ok_or("could not tell this user's name")?;
    let push = Config::load().map(|c| c.push).unwrap_or_default();
    let payload = payload(&name, &host, &user, url, &push);
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
        println!(
            "The phone connects to {url} and needs Tailscale, signed in as {}.",
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

/// The address the phone connects to, or why there is none yet.
fn tailnet_url(s: &TailscaleStatus) -> Result<&str, String> {
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
    match (&s.https.state, &s.url) {
        (HttpsState::Ready, Some(url)) => Ok(url),
        _ => Err("HTTPS is not ready on the tailnet yet: `thurm tailscale status` says why".into()),
    }
}

/// The host's tailnet name: the iOS app shows it, and needs a host in every entry.
fn host_of(s: &TailscaleStatus, url: &str) -> String {
    match &s.dns_name {
        Some(dns) if !dns.is_empty() => dns.trim_end_matches('.').to_owned(),
        _ => url
            .trim_start_matches("wss://")
            .split(['/', ':'])
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// What the code carries (`PairingPayload` in the iOS app): this host over Tailscale only, and
/// the push relay when this host's config has one.
fn payload(name: &str, host: &str, user: &str, url: &str, push: &PushConfig) -> serde_json::Value {
    let mut d = serde_json::json!({
        "v": 1,
        "name": name,
        "host": host,
        "user": user,
        "host_keys": [],
        "tailnet": { "url": url },
    });
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
    const QUIET: usize = 2;
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
    fn pairs_only_once_serving_https() {
        let s = serving();
        assert_eq!(
            tailnet_url(&s).unwrap(),
            "wss://bastion.tail1234.ts.net/v1/connect"
        );
        let mut provisioning = serving();
        provisioning.https.state = HttpsState::Provisioning;
        assert!(tailnet_url(&provisioning).unwrap_err().contains("HTTPS"));
        let off = TailscaleStatus::default();
        assert!(
            tailnet_url(&off)
                .unwrap_err()
                .contains("thurm tailscale on")
        );
        let login = TailscaleStatus {
            state: TailscaleState::NeedsLogin,
            ..Default::default()
        };
        assert!(tailnet_url(&login).unwrap_err().contains("sign"));
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
    }

    /// What the iOS app's `PairingPayload.parse` reads: v 1, a host, a user, and a tailnet
    /// address standing in for host keys.
    #[test]
    fn link_carries_the_tailnet_address() {
        let url = "wss://bastion.tail1234.ts.net/v1/connect";
        let p = payload(
            "bastion",
            "bastion.tail1234.ts.net",
            "me",
            url,
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
        let p = payload(
            "bastion",
            "bastion",
            "me",
            "wss://bastion/v1/connect",
            &push,
        );
        assert_eq!(p["relay"], "https://push.example");
        assert_eq!(p["code"], "abc");
        let no_code = PushConfig {
            code: String::new(),
            ..push
        };
        assert!(
            payload("b", "b", "me", "wss://b/v1/connect", &no_code)
                .get("code")
                .is_none()
        );
    }

    #[test]
    fn renders_a_square_code_with_a_quiet_zone() {
        let out = render("thurm://pair?d=eyJ2IjoxfQ").unwrap();
        let rows: Vec<&str> = out.lines().collect();
        let width = |r: &str| {
            r.trim_start_matches("\x1b[30;107m")
                .trim_end_matches("\x1b[0m")
                .chars()
                .count()
        };
        let size = width(rows[0]);
        assert!(rows.iter().all(|r| width(r) == size));
        assert_eq!(rows.len(), size.div_ceil(2));
        assert!(rows[0].contains("  "), "a blank quiet zone on top");
    }
}
