//! Files to and from a host for the app (`thurm_upload`, `thurm_download`): each runs on a
//! thread of its own and reports through a callback. See `thurm.h`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use thurm_remote::transfer::{self, Cancel, Downloaded, Host, Progress, TransferError, Upload};

use crate::{Ctx, client_ref, opt_str, thurm_client};

pub type thurm_transfer_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char)>;

static RUNNING: Mutex<Option<HashMap<u64, Cancel>>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(1);

fn report(cb: thurm_transfer_cb, ctx: Ctx, value: serde_json::Value) {
    if let (Some(cb), Ok(s)) = (cb, std::ffi::CString::new(value.to_string())) {
        unsafe { cb(ctx.0, s.as_ptr()) };
    }
}

/// Reports progress once per MiB and at the end, so a large file does not flood the
/// app with callbacks.
fn progress(cb: thurm_transfer_cb, ctx: Ctx) -> impl FnMut(Progress) {
    let mut last = None;
    move |p: Progress| {
        let step = p.done / (1 << 20);
        if last == Some(step) && p.done != p.total {
            return;
        }
        last = Some(step);
        report(cb, ctx, json!({ "done": p.done, "total": p.total }));
    }
}

fn finished(error: Option<&TransferError>) -> serde_json::Value {
    json!({
        "finished": true,
        "cancelled": matches!(error, Some(TransferError::Cancelled)),
        "unsupported": matches!(error, Some(TransferError::Unsupported)),
        "folder": matches!(error, Some(TransferError::Folder(_))),
        "not_found": matches!(error, Some(TransferError::NotFound(_))),
        "error": error.map(|e| e.to_string()),
    })
}

/// Runs `work` on a thread with a new transfer id it can be cancelled by; 0 when no thread
/// could start (the callback has then said so).
fn start(
    cb: thurm_transfer_cb,
    ctx: Ctx,
    work: impl FnOnce(&Cancel) -> serde_json::Value + Send + 'static,
) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let cancel = Cancel::default();
    RUNNING
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(id, cancel.clone());
    let spawned = std::thread::Builder::new()
        .name(format!("transfer-{id}"))
        .spawn(move || {
            let done = work(&cancel);
            if let Some(m) = RUNNING.lock().as_mut() {
                m.remove(&id);
            }
            report(cb, ctx, done);
        });
    if let Err(e) = spawned {
        if let Some(m) = RUNNING.lock().as_mut() {
            m.remove(&id);
        }
        report(
            cb,
            ctx,
            finished(Some(&TransferError::Local(format!(
                "cannot start the transfer: {e}"
            )))),
        );
        return 0;
    }
    id
}

fn cancel(id: u64) {
    if let Some(c) = RUNNING.lock().as_ref().and_then(|m| m.get(&id)) {
        c.cancel();
    }
}

#[derive(Deserialize)]
struct FileSpec {
    path: PathBuf,
    #[serde(default)]
    name: Option<String>,
}

/// The connection and its socket, for a thread to use after the call returns.
fn host_of(c: &thurm_client) -> Result<(std::sync::Arc<::thurm_client::Client>, PathBuf), String> {
    let socket = c
        .socket
        .clone()
        .ok_or("this connection carries no file transfers")?;
    Ok((c.client.clone(), socket))
}

/// Sends files to `pane`'s host as one batch: `files_json` is an array of `{"path": ...}`,
/// with `"name"` to send one under another name. Returns the transfer's id (for
/// `thurm_upload_cancel`), or 0 when it could not start; either way `cb` hears the end: on the
/// transfer's thread, or for a 0, on the calling thread before this returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_upload(
    client: *mut thurm_client,
    pane: u64,
    files_json: *const c_char,
    cb: thurm_transfer_cb,
    ctx: *mut c_void,
) -> u64 {
    let ctx = Ctx(ctx);
    let fail = |e: String| {
        report(cb, ctx, finished(Some(&TransferError::Local(e))));
        0
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return fail("not connected".into());
    };
    let specs: Vec<FileSpec> = match unsafe { opt_str(files_json) }.map(serde_json::from_str) {
        Some(Ok(specs)) => specs,
        Some(Err(e)) => return fail(format!("bad file list: {e}")),
        None => return fail("no file list".into()),
    };
    let files: Vec<Upload> = specs
        .into_iter()
        .map(|s| match s.name {
            Some(name) => Upload::Named { name, path: s.path },
            None => Upload::Path(s.path),
        })
        .collect();
    let (client, socket) = match host_of(c) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    start(cb, ctx, move |cancel| {
        let host = Host {
            client: &client,
            socket: &socket,
        };
        let sent = transfer::upload(host, pane, &files, cancel, progress(cb, ctx));
        finished(sent.as_ref().err())
    })
}

/// Stops upload `id`: nothing it sent is kept, and nothing is typed, if the daemon hears it
/// before the batch is complete. Once the paths are typed it is too late; the final callback
/// says what happened.
#[unsafe(no_mangle)]
pub extern "C" fn thurm_upload_cancel(id: u64) {
    cancel(id);
}

/// Fetches the file at `path` (as `pane` sees it) into `dest`, replacing what is there (use
/// `thurm_download_to_folder` for a folder others write to). With `if_version` (or NULL)
/// still the file's version, writes nothing (`"not_modified": true`). A file over `max_bytes`
/// (0 for no limit) is refused. The final callback carries `"info"`: the file's `name`, `path`,
/// `size`, `modified` and `version`. Returns the transfer's id, or 0 when it could not start
/// (the final callback then came on the calling thread, before this returns).
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn thurm_download(
    client: *mut thurm_client,
    pane: u64,
    path: *const c_char,
    dest: *const c_char,
    if_version: *const c_char,
    max_bytes: u64,
    cb: thurm_transfer_cb,
    ctx: *mut c_void,
) -> u64 {
    let ctx = Ctx(ctx);
    let fail = |e: String| {
        report(cb, ctx, finished(Some(&TransferError::Local(e))));
        0
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return fail("not connected".into());
    };
    let (Some(path), Some(dest)) = (unsafe { opt_str(path) }, unsafe { opt_str(dest) }) else {
        return fail("no path".into());
    };
    let (path, dest) = (path.to_owned(), PathBuf::from(dest));
    let if_version = unsafe { opt_str(if_version) }.map(str::to_owned);
    let max = (max_bytes > 0).then_some(max_bytes);
    let (client, socket) = match host_of(c) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    start(cb, ctx, move |cancel| {
        let host = Host {
            client: &client,
            socket: &socket,
        };
        let got = transfer::download(
            host,
            pane,
            &path,
            &dest,
            if_version.as_deref(),
            max,
            cancel,
            progress(cb, ctx),
        );
        match got {
            Ok(Downloaded::Saved(info)) | Ok(Downloaded::NotModified(info)) => saved(&info),
            Err(e) => finished(Some(&e)),
        }
    })
}

/// The final callback of a download that ended well, with what it fetched.
fn saved(info: &thurm_proto::DownloadInfo) -> serde_json::Value {
    let mut done = finished(None);
    done["not_modified"] = json!(info.not_modified);
    done["info"] = json!({
        "name": info.name,
        "path": info.path,
        "size": info.size,
        "modified": info.modified,
        "version": info.version,
    });
    done
}

/// Fetches the file at `path` (as `pane` sees it) into the folder `dir` as `name` (or
/// `name 2`… when taken): written in a private folder of its own there, then moved in by a
/// rename that never replaces a file, so nothing already in `dir` is touched. The final
/// callback's `saved_to` is where it went. Cancel with `thurm_download_cancel`. Returns the
/// transfer's id, or 0 when it could not start (the final callback then came on the calling
/// thread, before this returns).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_download_to_folder(
    client: *mut thurm_client,
    pane: u64,
    path: *const c_char,
    dir: *const c_char,
    name: *const c_char,
    cb: thurm_transfer_cb,
    ctx: *mut c_void,
) -> u64 {
    let ctx = Ctx(ctx);
    let fail = |e: String| {
        report(cb, ctx, finished(Some(&TransferError::Local(e))));
        0
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return fail("not connected".into());
    };
    let (Some(path), Some(dir)) = (unsafe { opt_str(path) }, unsafe { opt_str(dir) }) else {
        return fail("no path".into());
    };
    let name = unsafe { opt_str(name) }.unwrap_or_default().to_owned();
    let (path, dir) = (path.to_owned(), PathBuf::from(dir));
    let (client, socket) = match host_of(c) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    start(cb, ctx, move |cancel| {
        let host = Host {
            client: &client,
            socket: &socket,
        };
        match transfer::download_to_folder(
            host,
            pane,
            &path,
            &dir,
            &name,
            cancel,
            progress(cb, ctx),
        ) {
            Ok((info, dest)) => {
                let mut done = saved(&info);
                done["saved_to"] = json!(dest.to_string_lossy());
                done
            }
            Err(e) => finished(Some(&e)),
        }
    })
}

/// Stops download `id` while under way: what it wrote beside its destination is deleted, and a
/// file already at the destination is left as it was. A download already moved into place
/// stays; the final callback says so.
#[unsafe(no_mangle)]
pub extern "C" fn thurm_download_cancel(id: u64) {
    cancel(id);
}
