//! Byte-stream pre-filter in front of libghostty-vt's parser.
//!
//! Thurm handles APC strings (kitty graphics) and a handful of OSC sequences itself (cwd
//! reporting, desktop notifications, shell-integration prompt marks, clipboard access). The
//! filter splits the PTY stream into pass-through bytes and intercepted sequences, preserving
//! their order so that intercepted commands can be executed at the exact cursor position they
//! were emitted at.

use std::borrow::Cow;

/// OSC numbers Thurm handles (libghostty-vt has no way to report them, or Thurm does more).
const INTERCEPTED_OSC: &[u32] = &[
    52,   // clipboard reads (libghostty-vt drops them; writes are libghostty-vt's)
    99,   // kitty desktop notifications
    133,  // FinalTerm / shell integration prompt marks
    633,  // VS Code shell integration (same semantics as 133 for our purposes)
    3008, // context signalling (OSC 3008) – swallowed
];

/// Intercepted OSC numbers libghostty-vt gets too (it handles other parts of them).
const SHARED_OSC: &[u32] = &[52];

/// Longest OSC we buffer before giving up and passing it through untouched.
const MAX_OSC: usize = 8 * 1024 * 1024;
/// Longest APC we accept (kitty chunks are ≤ 4 KiB, but be generous).
const MAX_APC: usize = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// Bytes for the regular VT parser.
    Pass(Cow<'a, [u8]>),
    /// APC body (without `ESC _` and the terminator).
    Apc(Vec<u8>),
    /// OSC body (without `ESC ]` and the terminator), for an intercepted number.
    Osc(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC in ground state.
    Esc,
    /// Inside a CSI (passed through; watched for DECSTBM).
    Csi,
    /// OSC number not complete yet (buffered until we know whether it is ours).
    OscNum,
    /// Buffering an intercepted OSC.
    Osc,
    /// Saw ESC inside an intercepted OSC.
    OscEsc,
    /// Passing an OSC we don't intercept through as it arrives.
    OscPass,
    /// Saw ESC inside a passed-through OSC.
    OscPassEsc,
    /// APC control data not complete yet (buffered until we know whether it is ours).
    ApcCtl,
    /// Buffering an intercepted APC.
    Apc,
    ApcEsc,
    /// Passing an APC we don't intercept through as it arrives.
    ApcPass,
    /// Saw ESC inside a passed-through APC.
    ApcPassEsc,
    /// APC/OSC too long: discard until the terminator.
    Discard,
    DiscardEsc,
}

/// Longest APC control data we look at (kitty keys before the `;`).
const MAX_APC_CTL: usize = 1024;
/// Longest OSC number we look at (the intercepted ones have at most 4 digits).
const MAX_OSC_NUM: usize = 6;
/// Longest CSI parameter string we remember (DECSTBM needs a handful).
const MAX_CSI_PARAMS: usize = 16;

#[derive(Debug)]
pub struct StreamFilter {
    state: State,
    buf: Vec<u8>,
    /// Terminator used by the OSC being buffered (BEL vs ST) to replay it verbatim.
    osc_bel: bool,
    /// Parameters of the CSI being passed through.
    csi_params: Vec<u8>,
    /// The scroll region the stream set since the last [`StreamFilter::take_scroll_region`]
    /// (`Some(None)`: reset to the full screen).
    scroll_region: Option<Option<(u16, u16)>>,
    /// Take every APC out of the stream (kitty graphics turned off), not only transmissions
    /// libghostty-vt can't read itself.
    pub all_apc: bool,
    /// A chunked file / shared memory transmission is being taken out: its continuation
    /// chunks are too.
    media_more: bool,
}

impl Default for StreamFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            osc_bel: false,
            csi_params: Vec::new(),
            scroll_region: None,
            all_apc: false,
            media_more: false,
        }
    }

    /// The scroll region set (DECSTBM, or reset by RIS) in what was fed since the last call:
    /// `Some(None)` when it went back to the full screen.
    pub fn take_scroll_region(&mut self) -> Option<Option<(u16, u16)>> {
        self.scroll_region.take()
    }

    /// Split `input` into chunks, appending them to `out`.
    pub fn feed<'a>(&mut self, input: &'a [u8], out: &mut Vec<Chunk<'a>>) {
        let mut i = 0;
        // Start of the pending pass-through slice of `input`.
        let mut pass_start = 0;

        macro_rules! flush_pass {
            ($end:expr) => {
                if $end > pass_start {
                    out.push(Chunk::Pass(Cow::Borrowed(&input[pass_start..$end])));
                }
            };
        }
        // An ESC in pass-through bytes: it stays in the pending slice (most are CSIs, which
        // pass through) until what follows says otherwise.
        macro_rules! at_esc {
            () => {
                self.state = State::Esc;
                i += 1;
            };
        }
        // What precedes the ESC before `i`, when that ESC starts a sequence we take out.
        macro_rules! flush_before_esc {
            () => {
                if i > 0 {
                    flush_pass!(i - 1);
                }
            };
        }

        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Ground => match find_esc(&input[i..]) {
                    Some(off) => {
                        i += off;
                        match self.whole_csi(&input[i..]) {
                            Some(n) => i += n,
                            None => {
                                at_esc!();
                            }
                        }
                    }
                    None => i = input.len(),
                },
                State::Esc => {
                    match b {
                        b']' => {
                            flush_before_esc!();
                            self.state = State::OscNum;
                            self.buf.clear();
                            pass_start = i + 1;
                        }
                        b'_' => {
                            flush_before_esc!();
                            self.state = State::ApcCtl;
                            self.buf.clear();
                            pass_start = i + 1;
                        }
                        _ => {
                            self.state = State::Ground;
                            // The ESC may have been in a previous chunk.
                            if pass_start > i || i == 0 || input[i - 1] != 0x1b {
                                out.push(Chunk::Pass(Cow::Owned(vec![0x1b])));
                                pass_start = i;
                            }
                            // Otherwise ESC is part of the pending pass slice already.
                            match b {
                                0x1b => {
                                    // ESC ESC: flush the first, stay in Esc for the second.
                                    flush_pass!(i);
                                    pass_start = i;
                                    self.state = State::Esc;
                                }
                                b'[' => {
                                    self.state = State::Csi;
                                    self.csi_params.clear();
                                }
                                b'c' => {
                                    // RIS resets everything.
                                    self.scroll_region = Some(None);
                                }
                                _ => {}
                            }
                        }
                    }
                    i += 1;
                }
                State::Csi => {
                    if b.is_ascii_digit() || b == b';' {
                        let rest = &input[i..];
                        let n = rest
                            .iter()
                            .position(|c| !(c.is_ascii_digit() || *c == b';'))
                            .unwrap_or(rest.len());
                        let room = MAX_CSI_PARAMS.saturating_sub(self.csi_params.len());
                        self.csi_params.extend_from_slice(&rest[..n.min(room)]);
                        i += n;
                    } else {
                        if b == b'r' {
                            self.scroll_region = Some(parse_decstbm(&self.csi_params));
                        }
                        self.end_csi(b);
                        if b == 0x1b {
                            at_esc!();
                        } else {
                            i += 1;
                        }
                    }
                }
                State::OscNum => {
                    if b.is_ascii_digit() && self.buf.len() < MAX_OSC_NUM {
                        self.buf.push(b);
                        i += 1;
                    } else if b == 0x07 || b == 0x1b {
                        // Ends before any `;`: decide on the whole body, as before.
                        self.state = State::Osc;
                    } else if b == b';' && is_intercepted(&self.buf) {
                        self.buf.push(b);
                        self.state = State::Osc;
                        i += 1;
                    } else {
                        // Not ours: replay the prefix and pass the rest through as it comes.
                        let mut raw = Vec::with_capacity(self.buf.len() + 2);
                        raw.extend_from_slice(b"\x1b]");
                        raw.extend_from_slice(&self.buf);
                        out.push(Chunk::Pass(Cow::Owned(raw)));
                        self.buf.clear();
                        self.state = State::OscPass;
                    }
                    pass_start = i;
                }
                State::OscPass => match memchr::memchr2(0x07, 0x1b, &input[i..]) {
                    Some(off) => {
                        i += off;
                        if input[i] == 0x07 {
                            i += 1;
                            self.state = State::Ground;
                        } else {
                            self.state = State::OscPassEsc;
                            i += 1;
                        }
                    }
                    None => i = input.len(),
                },
                State::OscPassEsc => {
                    if b == b'\\' {
                        // ST: pass it, with the ESC if that came in an earlier chunk.
                        if pass_start > i || i == 0 || input[i - 1] != 0x1b {
                            out.push(Chunk::Pass(Cow::Owned(vec![0x1b])));
                            pass_start = i;
                        }
                        self.state = State::Ground;
                        i += 1;
                    } else {
                        // ESC ended the OSC and starts a new sequence.
                        self.state = State::Esc;
                    }
                }
                State::Osc => {
                    match memchr::memchr2(0x07, 0x1b, &input[i..]) {
                        Some(off) => {
                            self.buf.extend_from_slice(&input[i..i + off]);
                            i += off;
                            if input[i] == 0x07 {
                                self.osc_bel = true;
                                self.finish_osc(out);
                            } else {
                                self.state = State::OscEsc;
                            }
                            i += 1;
                        }
                        None => {
                            self.buf.extend_from_slice(&input[i..]);
                            i = input.len();
                        }
                    }
                    if self.state == State::Osc && self.buf.len() > MAX_OSC {
                        // Give up intercepting: replay what we have and pass the rest through.
                        let mut raw = b"\x1b]".to_vec();
                        raw.append(&mut self.buf);
                        out.push(Chunk::Pass(Cow::Owned(raw)));
                        self.state = State::OscPass;
                    }
                    pass_start = i;
                }
                State::OscEsc => {
                    if b == b'\\' {
                        self.osc_bel = false;
                        self.finish_osc(out);
                        i += 1;
                        pass_start = i;
                    } else {
                        // ESC aborted the OSC: replay it raw and reprocess ESC + b.
                        let mut raw = b"\x1b]".to_vec();
                        raw.append(&mut self.buf);
                        out.push(Chunk::Pass(Cow::Owned(raw)));
                        out.push(Chunk::Pass(Cow::Owned(vec![0x1b])));
                        self.state = State::Ground;
                        pass_start = i;
                        if b == b']' || b == b'_' {
                            // Pop the ESC we just emitted and treat this as a new sequence.
                            out.pop();
                            self.state = State::Esc;
                        }
                    }
                }
                State::ApcCtl => {
                    let rest = &input[i..];
                    let room = MAX_APC_CTL.saturating_sub(self.buf.len());
                    let scan = &rest[..rest.len().min(room)];
                    match memchr::memchr3(b';', 0x1b, 0x07, scan) {
                        Some(off) => {
                            self.buf.extend_from_slice(&scan[..off]);
                            i += off;
                            let semicolon = input[i] == b';';
                            if self.takes_apc() {
                                self.state = State::Apc;
                                if semicolon {
                                    self.buf.push(b';');
                                    i += 1;
                                }
                            } else {
                                self.pass_apc_prefix(out);
                            }
                        }
                        None if scan.len() < rest.len() => {
                            // Control data this long is not a transmission we read.
                            self.buf.extend_from_slice(scan);
                            i += scan.len();
                            self.pass_apc_prefix(out);
                        }
                        None => {
                            self.buf.extend_from_slice(scan);
                            i = input.len();
                        }
                    }
                    pass_start = i;
                }
                State::ApcPass => match memchr::memchr(0x1b, &input[i..]) {
                    Some(off) => {
                        i += off + 1;
                        self.state = State::ApcPassEsc;
                    }
                    None => i = input.len(),
                },
                State::ApcPassEsc => {
                    if b == b'\\' {
                        // ST: pass it, with the ESC if that came in an earlier chunk.
                        if pass_start > i || i == 0 || input[i - 1] != 0x1b {
                            out.push(Chunk::Pass(Cow::Owned(vec![0x1b])));
                            pass_start = i;
                        }
                        self.state = State::Ground;
                        i += 1;
                    } else {
                        // ESC ended the APC and starts a new sequence.
                        self.state = State::Esc;
                    }
                }
                State::Apc => {
                    match memchr::memchr3(0x1b, 0x07, 0x9c, &input[i..]) {
                        Some(off) => {
                            self.buf.extend_from_slice(&input[i..i + off]);
                            i += off;
                            if input[i] == 0x1b {
                                self.state = State::ApcEsc;
                            } else {
                                out.push(Chunk::Apc(std::mem::take(&mut self.buf)));
                                self.state = State::Ground;
                            }
                            i += 1;
                        }
                        None => {
                            self.buf.extend_from_slice(&input[i..]);
                            i = input.len();
                        }
                    }
                    if self.state == State::Apc && self.buf.len() > MAX_APC {
                        self.buf = Vec::new();
                        self.state = State::Discard;
                    }
                    pass_start = i;
                }
                State::ApcEsc => {
                    if b == b'\\' {
                        out.push(Chunk::Apc(std::mem::take(&mut self.buf)));
                        self.state = State::Ground;
                        i += 1;
                    } else {
                        // Aborted APC: drop it, reprocess ESC + b.
                        self.buf.clear();
                        self.state = State::Esc;
                        out.push(Chunk::Pass(Cow::Owned(Vec::new())));
                        out.pop();
                    }
                    pass_start = i;
                }
                State::Discard => {
                    match memchr::memchr2(0x1b, 0x07, &input[i..]) {
                        Some(off) => {
                            i += off;
                            self.state = if input[i] == 0x07 {
                                State::Ground
                            } else {
                                State::DiscardEsc
                            };
                            i += 1;
                        }
                        None => i = input.len(),
                    }
                    pass_start = i;
                }
                State::DiscardEsc => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Discard
                    };
                    i += 1;
                    pass_start = i;
                }
            }
        }

        match self.state {
            State::Ground | State::Csi | State::OscPass | State::ApcPass => {
                flush_pass!(input.len())
            }
            State::Esc | State::OscPassEsc | State::ApcPassEsc => {
                // Pending ESC at the end: emit everything before it; the ESC itself is
                // re-emitted once we know what follows.
                let end = input.len() - 1;
                if pass_start <= end && input.get(end) == Some(&0x1b) {
                    flush_pass!(end);
                }
            }
            _ => {}
        }
    }

    /// Fast path for a complete, well-formed CSI at the start of `bytes` (which starts with
    /// ESC): its length, after noting DECSTBM. `None` leaves it to the state
    /// machine (split across chunks, or bytes the state machine treats specially).
    fn whole_csi(&mut self, bytes: &[u8]) -> Option<usize> {
        if bytes.get(1) != Some(&b'[') {
            return None;
        }
        // One pass: parameter bytes, then intermediates, then the final byte.
        let mut j = 2;
        let mut numeric = true;
        while let Some(&c) = bytes.get(j) {
            if !(0x30..=0x3f).contains(&c) {
                break;
            }
            numeric &= c <= b';' && c != b':';
            j += 1;
        }
        let params_end = j;
        while let Some(&c) = bytes.get(j) {
            if !(0x20..=0x2f).contains(&c) {
                break;
            }
            j += 1;
        }
        let fin = *bytes.get(j)?;
        if !(0x40..=0x7e).contains(&fin) {
            return None;
        }
        if numeric && fin == b'r' && params_end == j {
            let params = &bytes[2..params_end.min(2 + MAX_CSI_PARAMS)];
            self.scroll_region = Some(parse_decstbm(params));
        }
        Some(j + 1)
    }

    /// Whether the APC whose control data is in `buf` is taken out of the stream: kitty
    /// graphics transmissions from a file, temporary file or shared memory (and their
    /// continuation chunks), which Thurm reads itself; everything when graphics are off.
    fn takes_apc(&mut self) -> bool {
        if self.all_apc {
            return true;
        }
        let Some(ctl) = self.buf.strip_prefix(b"G") else {
            return false;
        };
        let (mut medium, mut more) = (b'd', false);
        for kv in ctl.split(|&b| b == b',') {
            match kv {
                [b't', b'=', m, ..] => medium = *m,
                [b'm', b'=', rest @ ..] => more = rest == b"1",
                _ => {}
            }
        }
        let media = self.media_more || matches!(medium, b'f' | b't' | b's');
        if media {
            self.media_more = more;
        }
        media
    }

    /// Not ours: replay the buffered start of the APC and pass the rest through as it comes.
    fn pass_apc_prefix(&mut self, out: &mut Vec<Chunk<'_>>) {
        let mut raw = Vec::with_capacity(self.buf.len() + 2);
        raw.extend_from_slice(b"\x1b_");
        raw.extend_from_slice(&self.buf);
        out.push(Chunk::Pass(Cow::Owned(raw)));
        self.buf.clear();
        self.state = State::ApcPass;
    }

    /// A CSI ended with `b` (its final byte, or whatever interrupted it).
    fn end_csi(&mut self, b: u8) {
        self.state = if b == 0x1b { State::Esc } else { State::Ground };
    }

    fn finish_osc(&mut self, out: &mut Vec<Chunk<'_>>) {
        self.state = State::Ground;
        let body = std::mem::take(&mut self.buf);
        let num = osc_number(&body);
        if num.is_some_and(|n| INTERCEPTED_OSC.contains(&n)) {
            let raw = num.is_some_and(|n| SHARED_OSC.contains(&n)).then(|| {
                let mut raw = Vec::with_capacity(body.len() + 4);
                raw.extend_from_slice(b"\x1b]");
                raw.extend_from_slice(&body);
                raw.extend_from_slice(if self.osc_bel { b"\x07" } else { b"\x1b\\" });
                raw
            });
            out.push(Chunk::Osc(body));
            if let Some(raw) = raw {
                out.push(Chunk::Pass(Cow::Owned(raw)));
            }
        } else {
            let mut raw = Vec::with_capacity(body.len() + 4);
            raw.extend_from_slice(b"\x1b]");
            raw.extend_from_slice(&body);
            if self.osc_bel {
                raw.push(0x07);
            } else {
                raw.extend_from_slice(b"\x1b\\");
            }
            out.push(Chunk::Pass(Cow::Owned(raw)));
        }
    }
}

/// Offset of the next ESC. Escapes usually come a few bytes apart (SGR runs), where a plain
/// loop beats memchr's setup; long runs of text go to memchr.
#[inline]
fn find_esc(bytes: &[u8]) -> Option<usize> {
    let head = bytes.len().min(16);
    if let Some(off) = bytes[..head].iter().position(|&c| c == 0x1b) {
        return Some(off);
    }
    memchr::memchr(0x1b, &bytes[head..]).map(|off| off + head)
}

/// DECSTBM parameters to a scroll region (`None`: the full screen).
fn parse_decstbm(params: &[u8]) -> Option<(u16, u16)> {
    let p = String::from_utf8_lossy(params);
    let mut it = p.split(';').map(|v| v.parse::<u16>().ok());
    let top = it.next().flatten().unwrap_or(1).max(1);
    let bottom = it.next().flatten();
    if top == 1 && bottom.is_none() {
        return None;
    }
    bottom.map(|b| (top, b)).filter(|&(t, b)| b > t)
}

fn osc_number(body: &[u8]) -> Option<u32> {
    let num_end = body.iter().position(|&b| b == b';').unwrap_or(body.len());
    std::str::from_utf8(&body[..num_end]).ok()?.parse().ok()
}

fn is_intercepted(body: &[u8]) -> bool {
    osc_number(body).is_some_and(|n| INTERCEPTED_OSC.contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `input` split at every possible position and in one piece; all must agree.
    fn run(input: &[u8]) -> (Vec<u8>, Vec<Chunk<'static>>) {
        let collect = |parts: &[&[u8]]| {
            let mut f = StreamFilter::new();
            let mut pass = Vec::new();
            let mut seqs = Vec::new();
            for p in parts {
                let mut out = Vec::new();
                f.feed(p, &mut out);
                for c in out {
                    match c {
                        Chunk::Pass(b) => pass.extend_from_slice(&b),
                        Chunk::Apc(a) => seqs.push(Chunk::Apc(a)),
                        Chunk::Osc(o) => seqs.push(Chunk::Osc(o)),
                    }
                }
            }
            (pass, seqs)
        };
        let whole = collect(&[input]);
        for split in 0..=input.len() {
            let (a, b) = input.split_at(split);
            assert_eq!(collect(&[a, b]), whole, "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(collect(&bytes), whole, "byte by byte");
        whole
    }

    #[test]
    fn plain_text_passes() {
        let (pass, seqs) = run(b"hello \x1b[31mworld\x1b[0m\r\n");
        assert_eq!(pass, b"hello \x1b[31mworld\x1b[0m\r\n");
        assert!(seqs.is_empty());
    }

    #[test]
    fn direct_apc_passes_through() {
        for input in [
            &b"a\x1b_Gi=1,a=q;AAAA\x1b\\b"[..],
            b"a\x1b_Ga=d,d=i,i=7\x1b\\b",
            b"a\x1b_Ga=T,t=d,m=1;AAAA\x1b\\\x1b_Gm=0;BBBB\x1b\\b",
            b"a\x1b_other\x1b\\b",
            // Aborted by another sequence.
            b"a\x1b_Gi=1;AA\x1b[1mb",
        ] {
            let (pass, seqs) = run(input);
            assert_eq!(pass, input.to_vec(), "{input:?}");
            assert!(seqs.is_empty(), "{input:?}");
        }
    }

    #[test]
    fn file_transmissions_are_extracted() {
        let (pass, seqs) =
            run(b"a\x1b_Ga=T,t=f,m=1;L3Rt\x1b\\\x1b_Gm=0;cA==\x1b\\b\x1b_Ga=p,i=1\x1b\\");
        assert_eq!(pass, b"ab\x1b_Ga=p,i=1\x1b\\");
        assert_eq!(
            seqs,
            vec![
                Chunk::Apc(b"Ga=T,t=f,m=1;L3Rt".to_vec()),
                Chunk::Apc(b"Gm=0;cA==".to_vec())
            ]
        );
        for m in *b"st" {
            let input = [&b"\x1b_Ga=t,t="[..], &[m], b";eA==\x1b\\"].concat();
            let (pass, seqs) = run(&input);
            assert!(pass.is_empty());
            assert_eq!(seqs.len(), 1);
        }
    }

    #[test]
    fn all_apc_when_graphics_are_off() {
        let mut f = StreamFilter::new();
        f.all_apc = true;
        let mut out = Vec::new();
        f.feed(b"a\x1b_Gi=1;AAAA\x1b\\b", &mut out);
        assert_eq!(
            out,
            vec![
                Chunk::Pass(Cow::Borrowed(&b"a"[..])),
                Chunk::Apc(b"Gi=1;AAAA".to_vec()),
                Chunk::Pass(Cow::Borrowed(&b"b"[..])),
            ]
        );
    }

    #[test]
    fn intercepted_osc_st_and_bel() {
        let (pass, seqs) = run(b"x\x1b]99;;hi\x07y\x1b]133;A\x1b\\z");
        assert_eq!(pass, b"xyz");
        assert_eq!(
            seqs,
            vec![
                Chunk::Osc(b"99;;hi".to_vec()),
                Chunk::Osc(b"133;A".to_vec())
            ]
        );
        // Shared with libghostty-vt: taken and passed on too.
        let (pass, seqs) = run(b"x\x1b]52;c;?\x1b\\y");
        assert_eq!(pass, b"x\x1b]52;c;?\x1b\\y");
        assert_eq!(seqs, vec![Chunk::Osc(b"52;c;?".to_vec())]);
    }

    #[test]
    fn other_osc_passes_verbatim() {
        let input = b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\";
        let (pass, seqs) = run(input);
        assert_eq!(pass, input.to_vec());
        assert!(seqs.is_empty());
    }

    #[test]
    fn aborted_osc_is_replayed() {
        let (pass, seqs) = run(b"\x1b]0;ti\x1b[1m");
        assert_eq!(pass, b"\x1b]0;ti\x1b[1m");
        assert!(seqs.is_empty());
    }

    #[test]
    fn double_escape() {
        let (pass, seqs) = run(b"\x1b\x1b]99;;hi\x07!");
        assert_eq!(pass, b"\x1b!");
        assert_eq!(seqs, vec![Chunk::Osc(b"99;;hi".to_vec())]);
    }

    #[test]
    fn osc_aborted_by_new_osc() {
        let (pass, seqs) = run(b"\x1b]0;a\x1b]99;;hello\x1b\\");
        assert_eq!(pass, b"\x1b]0;a");
        assert_eq!(seqs, vec![Chunk::Osc(b"99;;hello".to_vec())]);
    }

    #[test]
    fn utf8_passes() {
        let (pass, _) = run("żółw ⇒ 🦀".as_bytes());
        assert_eq!(pass, "żółw ⇒ 🦀".as_bytes());
    }
    #[test]
    fn tracks_scroll_region_across_chunks() {
        let mut f = StreamFilter::new();
        let mut feed = |b: &[u8]| {
            f.feed(b, &mut Vec::new());
            f.take_scroll_region()
        };
        assert_eq!(feed(b"plain text without escapes"), None);
        assert_eq!(feed(b"abc\x1b[5;20rdef"), Some(Some((5, 20))));
        // Split mid-sequence, including right after the ESC.
        assert_eq!(feed(b"xyz\x1b"), None);
        assert_eq!(feed(b"[2;1"), None);
        assert_eq!(feed(b"0r tail"), Some(Some((2, 10))));
        assert_eq!(feed(b"\x1b[r"), Some(None));
        assert_eq!(feed(b"\x1b[3;9r\x1bc"), Some(None));
        // Other sequences leave it alone.
        assert_eq!(feed(b"\x1b[1;31mred\x1b[0m\x1b[2J\x1b[?25l"), None);
    }

    #[test]
    fn other_osc_streams_through() {
        for input in [
            &b"a\x1b]6;some long data\x07b"[..],
            b"a\x1b]6;data\x1b\\b",
            b"a\x1b]12345678;x\x07b",
            b"a\x1b];no number\x07b",
            b"a\x1b]L?\x1b\\b",
            // Aborted by another sequence, or by an OSC we intercept.
            b"a\x1b]6;xy\x1b[1mb",
            b"\x1b]6;xy\x1b]99;;t\x07z",
            b"a\x1b]7;file://h/tmp\x07b",
            b"a\x1b]1337;CurrentDir=/tmp\x07b",
        ] {
            let (pass, seqs) = run(input);
            let expected: Vec<u8> = if input.starts_with(b"\x1b]6;xy\x1b]99") {
                b"\x1b]6;xyz".to_vec()
            } else {
                input.to_vec()
            };
            assert_eq!(pass, expected, "{input:?}");
            assert_eq!(seqs.is_empty(), !input.ends_with(b"\x07z"), "{input:?}");
        }
        // Passed through without being buffered.
        let mut f = StreamFilter::new();
        let mut out = Vec::new();
        f.feed(b"\x1b]6;", &mut out);
        out.clear();
        f.feed(b"payload", &mut out);
        assert_eq!(out, vec![Chunk::Pass(Cow::Borrowed(&b"payload"[..]))]);
    }
}
