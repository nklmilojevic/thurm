//! The iOS app's connections: a paired host's daemon over an in-process ssh session
//! (`thurm-ssh`), with the device key held by the app (see `thurm_device_key`), or over
//! Tailscale where the daemon serves there (`thurm-tailnet-client`), and a host without Thurm,
//! served by a daemon in the app (`thurm-direct`).

use std::ffi::{c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use crate::{
    Ctx, client_ref, into_c, new_connection, opt_str, thurm_client, thurm_event_cb, thurm_frame_cb,
};

/// The device's ssh key, held by the app (a Secure Enclave P-256 key). `sign` writes the
/// 64-byte `r || s` ECDSA/SHA-256 signature of `data` to `sig` and returns true, or false when
/// the user declined; it is called on a background thread and may block (Face ID).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct thurm_device_key {
    pub ctx: *mut c_void,
    /// Uncompressed SEC1 point, 65 bytes.
    pub public_sec1: *const u8,
    pub public_len: usize,
    pub sign: Option<
        unsafe extern "C" fn(ctx: *mut c_void, data: *const u8, len: usize, sig: *mut u8) -> bool,
    >,
}

struct AppKey {
    ctx: Ctx,
    public: Vec<u8>,
    sign: unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut u8) -> bool,
}

impl thurm_ssh::DeviceKey for AppKey {
    fn public_sec1(&self) -> Vec<u8> {
        self.public.clone()
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let mut sig = vec![0u8; 64];
        if unsafe { (self.sign)(self.ctx.0, data.as_ptr(), data.len(), sig.as_mut_ptr()) } {
            Ok(sig)
        } else {
            Err("the device key did not sign (declined or unavailable)".into())
        }
    }
}

unsafe fn app_key(key: &thurm_device_key) -> Result<Arc<AppKey>, String> {
    let sign = key.sign.ok_or("no signing function")?;
    if key.public_sec1.is_null() || key.public_len == 0 {
        return Err("no public key".into());
    }
    let public = unsafe { std::slice::from_raw_parts(key.public_sec1, key.public_len) }.to_vec();
    Ok(Arc::new(AppKey {
        ctx: Ctx(key.ctx),
        public,
        sign,
    }))
}

fn set_err(err: *mut *mut c_char, e: String) {
    if !err.is_null() {
        unsafe { *err = into_c(e) };
    }
}

/// `thurm_ssh_connect`'s failure, as the JSON `thurm_direct_connect` uses.
fn ssh_failure(kind: &str, message: impl std::fmt::Display) -> String {
    serde_json::json!({"kind": kind, "message": message.to_string()}).to_string()
}

/// Connects to a paired host's daemon. `target_json`:
/// `{"host","port","user","host_keys":["SHA256:…"],"socket"?}`. Blocks while connecting (ssh,
/// probe, daemon start, Hello), so call it off the main thread. NULL with `err` set on failure
/// (free it): JSON `{"kind","message"}`, `kind` being `"key_rejected"` (the host does not have
/// the device's key yet), `"not_installed"` (no `thurm` there) or `"failed"`. Otherwise a client
/// like `thurm_connect`'s, freed with `thurm_disconnect`, which also closes the ssh session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_ssh_connect(
    target_json: *const c_char,
    key: thurm_device_key,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let target = match unsafe { opt_str(target_json) }
        .ok_or_else(|| "no target".to_owned())
        .and_then(|j| serde_json::from_str::<thurm_ssh::Target>(j).map_err(|e| e.to_string()))
    {
        Ok(t) => t,
        Err(e) => {
            set_err(err, ssh_failure("failed", format!("bad target: {e}")));
            return std::ptr::null_mut();
        }
    };
    let key = match unsafe { app_key(&key) } {
        Ok(k) => k,
        Err(e) => {
            set_err(err, ssh_failure("failed", e));
            return std::ptr::null_mut();
        }
    };
    let name = unsafe { opt_str(client_name) }
        .unwrap_or("Thurm iOS")
        .to_owned();
    let connected = new_connection(on_event, on_frame, ctx, |on_ev, on_disc| {
        let remote = thurm_ssh::connect(&target, key, &name, on_ev, on_disc).map_err(|e| {
            let kind = match &e {
                thurm_ssh::ConnectError::KeyRejected => "key_rejected",
                thurm_ssh::ConnectError::NotInstalled => "not_installed",
                _ => "failed",
            };
            ssh_failure(kind, e)
        })?;
        let transport: Box<dyn std::any::Any + Send> = Box::new(remote.session);
        Ok((remote.client, Some(transport)))
    });
    match connected {
        Ok(c) => c,
        Err(e) => {
            set_err(err, e);
            std::ptr::null_mut()
        }
    }
}

/// Connects to a daemon served over Tailscale: `url` is the pairing code's
/// `wss://<device>.<tailnet>.ts.net/v1/connect`. The phone's own Tailscale carries it; the daemon
/// admits this device by its Tailscale account. Blocks while connecting (at most a few seconds:
/// a device whose Tailscale is off fails fast, to fall back to ssh), so call it off the main
/// thread. NULL with `err` set on failure (free it); otherwise a client like `thurm_connect`'s,
/// freed with `thurm_disconnect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_tailnet_connect(
    url: *const c_char,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let Some(url) = (unsafe { opt_str(url) }).map(str::to_owned) else {
        set_err(err, "no address".into());
        return std::ptr::null_mut();
    };
    let name = unsafe { opt_str(client_name) }
        .unwrap_or("Thurm iOS")
        .to_owned();
    let connected = new_connection(on_event, on_frame, ctx, |on_ev, on_disc| {
        let remote =
            thurm_tailnet_client::connect(&url, &name, &[thurm_proto::CAP_MOBILE], on_ev, on_disc)
                .map_err(|e| e.to_string())?;
        let transport: Box<dyn std::any::Any + Send> = Box::new(remote.session);
        Ok((remote.client, Some(transport)))
    });
    match connected {
        Ok(c) => c,
        Err(e) => {
            set_err(err, e);
            std::ptr::null_mut()
        }
    }
}

/// Like `thurm_tailnet_connect`, over `fd`: a stream socket the app already connected to the
/// daemon's device on port 443, with Tailscale embedded in the app (TailscaleKit's
/// `tailscale_dial`). `url` names the daemon (its host is checked against the certificate).
/// Takes `fd` over: it is closed when the connection ends, or now when connecting fails.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_tailnet_connect_fd(
    fd: c_int,
    url: *const c_char,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    if fd < 0 {
        set_err(err, "no socket".into());
        return std::ptr::null_mut();
    }
    // SAFETY: the caller hands over an open fd it no longer uses.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let Some(url) = (unsafe { opt_str(url) }).map(str::to_owned) else {
        set_err(err, "no address".into());
        return std::ptr::null_mut();
    };
    let name = unsafe { opt_str(client_name) }
        .unwrap_or("Thurm iOS")
        .to_owned();
    let connected = new_connection(on_event, on_frame, ctx, |on_ev, on_disc| {
        let remote = thurm_tailnet_client::connect_fd(
            fd,
            &url,
            &name,
            &[thurm_proto::CAP_MOBILE],
            on_ev,
            on_disc,
        )
        .map_err(|e| e.to_string())?;
        let transport: Box<dyn std::any::Any + Send> = Box::new(remote.session);
        Ok((remote.client, Some(transport)))
    });
    match connected {
        Ok(c) => c,
        Err(e) => {
            set_err(err, e);
            std::ptr::null_mut()
        }
    }
}

/// Connects to a host without Thurm, whose shells become this client's panes (`CreatePane`
/// opens one). `target_json`: `{"host","port","user","host_keys":["SHA256:…"],"mode",
/// "mosh_server"?,"mosh_ports"?}`, `mode` being `"mosh"` (falls back to ssh), `"mosh_only"` or
/// `"ssh"`. Blocks while connecting (ssh), so call it off the main thread. NULL with `err` set
/// on failure (free it): JSON `{"kind","message","fingerprint"?}`, `kind` being
/// `"unknown_host_key"` (ask the user to trust `fingerprint`), `"host_key_changed"`,
/// `"key_rejected"` or `"failed"`. Otherwise a client like `thurm_connect`'s, freed with
/// `thurm_disconnect`, which also ends the host's shells.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_direct_connect(
    target_json: *const c_char,
    key: thurm_device_key,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let failed =
        |message: String| serde_json::json!({"kind": "failed", "message": message}).to_string();
    let target = match unsafe { opt_str(target_json) }
        .ok_or_else(|| "no target".to_owned())
        .and_then(|j| {
            serde_json::from_str::<thurm_direct::DirectTarget>(j).map_err(|e| e.to_string())
        }) {
        Ok(t) => t,
        Err(e) => {
            set_err(err, failed(format!("bad target: {e}")));
            return std::ptr::null_mut();
        }
    };
    let key = match unsafe { app_key(&key) } {
        Ok(k) => k,
        Err(e) => {
            set_err(err, failed(e));
            return std::ptr::null_mut();
        }
    };
    let name = unsafe { opt_str(client_name) }
        .unwrap_or("Thurm iOS")
        .to_owned();
    let connected = new_connection(on_event, on_frame, ctx, |on_ev, on_disc| {
        let (stream, direct) = thurm_direct::serve(target, key).map_err(|e| {
            let (kind, fingerprint) = match &e {
                thurm_direct::DirectError::UnknownHostKey(k) => ("unknown_host_key", Some(k)),
                thurm_direct::DirectError::HostKeyChanged(k) => ("host_key_changed", Some(k)),
                thurm_direct::DirectError::KeyRejected => ("key_rejected", None),
                thurm_direct::DirectError::Failed(_) => ("failed", None),
            };
            serde_json::json!({"kind": kind, "message": e.to_string(), "fingerprint": fingerprint})
                .to_string()
        })?;
        let client = ::thurm_client::Client::from_stream(
            stream,
            &name,
            false,
            &[thurm_proto::CAP_MOBILE],
            on_ev,
            on_disc,
        )
        .map_err(|e| failed(e.to_string()))?;
        let transport: Box<dyn std::any::Any + Send> = Box::new(direct);
        Ok((client, Some(transport)))
    });
    match connected {
        Ok(c) => c,
        Err(e) => {
            set_err(err, e);
            std::ptr::null_mut()
        }
    }
}

/// The network changed or the app came back to the foreground: a client of
/// `thurm_direct_connect` moves its mosh sessions to new sockets now, instead of after
/// seconds of silence. Does nothing for other clients.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_direct_wake(client: *mut thurm_client) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    if let Some(direct) = c
        ._transport
        .as_ref()
        .and_then(|t| t.downcast_ref::<thurm_direct::Direct>())
    {
        direct.wake();
    }
}

/// The device key as an `authorized_keys` line with `comment`, for pairing. Free the result.
/// NULL when the key is unusable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_ssh_authorized_key(
    key: thurm_device_key,
    comment: *const c_char,
) -> *mut c_char {
    let Ok(key) = (unsafe { app_key(&key) }) else {
        return std::ptr::null_mut();
    };
    let comment = unsafe { opt_str(comment) }.unwrap_or("thurm-ios");
    match thurm_ssh::authorized_key(key.as_ref(), comment) {
        Ok(line) => into_c(line),
        Err(_) => std::ptr::null_mut(),
    }
}
