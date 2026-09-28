//! The part of the kitty graphics protocol Thurm handles itself: transmissions from a file,
//! a temporary file or shared memory. libghostty-vt implements the protocol, but every copy
//! of a pane's terminal (the daemon's, each app's) parses the same stream, and a temporary
//! file or shared memory object can be read only once (it is deleted after reading). So Thurm
//! reads it, and turns the command into a direct transmission of the bytes it read, which
//! every copy then parses the same way.

use std::io::Read;
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
#[derive(Default)]
pub struct MediaReader {
    /// Control data and base64 payload of a chunked transmission so far.
    pending: Option<(Vec<u8>, Vec<u8>)>,
}

/// Base64 bytes per chunk of the direct transmissions we write (the protocol's 4096).
const CHUNK: usize = 4096;

impl MediaReader {
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
        match read(&ctl, &payload) {
            Ok(raw) => Media::Direct(direct_transmission(&ctl, &raw)),
            Err(e) => reply(&ctl, e).map_or(Media::Drop, Media::Reply),
        }
    }
}

type Err = (&'static str, String);

fn err(code: &'static str, msg: impl Into<String>) -> Err {
    (code, msg.into())
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
fn read(ctl: &[u8], payload: &[u8]) -> Result<Vec<u8>, Err> {
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
        Some(b'f') => read_file(&name, offset, size),
        Some(b't') => {
            if !is_temp_graphics_file(&name) {
                return Err(err("EPERM", "not a temporary graphics file"));
            }
            let bytes = read_file(&name, offset, size);
            let _ = std::fs::remove_file(&name);
            bytes
        }
        Some(b's') => read_shm(&name, offset, size),
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
    Some(format!("\x1b_G{};{code}:{msg}\x1b\\", fields.join(",")).into_bytes())
}

/// Reads and unlinks a POSIX shared memory object (`t=s`), as the protocol requires.
fn read_shm(name: &str, offset: usize, size: Option<usize>) -> Result<Vec<u8>, Err> {
    if name.is_empty() || name.len() > 255 || name[1..].contains('/') {
        return Err(err("EINVAL", "bad shared memory name"));
    }
    let cname =
        std::ffi::CString::new(name).map_err(|_| err("EINVAL", "bad shared memory name"))?;
    let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(err(
            "EBADF",
            format!("shm_open {name}: {}", std::io::Error::last_os_error()),
        ));
    }
    let result = (|| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(err("EBADF", "fstat failed"));
        }
        let total = st.st_size.max(0) as usize;
        if offset > total {
            return Err(err("EINVAL", "offset past the end of the shared memory"));
        }
        let len = size.unwrap_or(total - offset).min(total - offset);
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

fn read_file(path: &str, offset: usize, size: Option<usize>) -> Result<Vec<u8>, Err> {
    let mut f = std::fs::File::open(path).map_err(|e| err("EBADF", format!("{path}: {e}")))?;
    let meta = f.metadata().map_err(|e| err("EBADF", e.to_string()))?;
    if !meta.is_file() {
        return Err(err("EBADF", "not a regular file"));
    }
    if meta.len() > 512 * 1024 * 1024 {
        return Err(err("EFBIG", "file too large"));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| err("EBADF", e.to_string()))?;
    let start = offset.min(buf.len());
    let end = size.map_or(buf.len(), |s| (start + s).min(buf.len()));
    buf.truncate(end);
    buf.drain(..start);
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
    if w > 10000 || h > 10000 {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    let px = (w * h) as usize;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
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
