//! What a pane's terminal sends: keys (legacy, CSI/SS3 and kitty CSI u), bracketed paste, SGR
//! mouse reports and focus reports, as Neovim input.

/// A mouse report, for `nvim_input_mouse(button, action, modifier, grid, row, col)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mouse {
    /// "left", "right", "middle", "wheel", "move" (no button held), or "x1"/"x2".
    pub button: &'static str,
    /// "press", "drag", "release"; for "wheel": "up", "down", "left", "right"; "" for "move".
    pub action: &'static str,
    /// Neovim modifier prefixes, e.g. "C-A-S-" (Ctrl, Alt, Shift), or "".
    pub modifier: String,
    /// 0-based cell of the pane.
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InputEvent {
    /// Keys in Neovim notation (`<C-w>`, `<lt>`, `a`, ...), for `nvim_input`.
    Keys(String),
    /// Bracketed paste: `PasteStart`, any number of `Paste` chunks, `PasteEnd`
    /// (`nvim_paste` phases 1, 2 and 3).
    PasteStart,
    Paste(String),
    PasteEnd,
    Mouse(Mouse),
    FocusIn,
    FocusOut,
}

const MOD_SHIFT: u8 = 1;
const MOD_ALT: u8 = 2;
const MOD_CTRL: u8 = 4;
const MOD_SUPER: u8 = 8;
const MAX_SEQ: usize = 64;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Utf8 {
        len: u8,
        alt: bool,
    },
    Csi,
    Ss3,
    Str {
        esc: bool,
    },
}

/// Turns the bytes a pane's terminal sends into [`InputEvent`]s. Keeps state across reads: a
/// sequence may be split between two.
#[derive(Default)]
pub struct InputParser {
    state: State,
    /// Bytes of the UTF-8 character or CSI sequence being read.
    seq: Vec<u8>,
    overflow: bool,
    /// The run of keys since the last non-key event.
    keys: String,
    out: Vec<InputEvent>,
    paste: bool,
    /// Paste bytes already known not to be part of the end marker.
    paste_body: Vec<u8>,
    /// Trailing paste bytes that may still start the end marker.
    paste_pending: Vec<u8>,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// The events in `bytes`, after what earlier reads left unfinished.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        for &b in bytes {
            self.byte(b);
        }
        if self.state == State::Esc {
            self.state = State::Ground;
            self.key(0, Key::Named("Esc"));
        }
        self.flush_paste();
        self.flush_keys();
        std::mem::take(&mut self.out)
    }

    fn byte(&mut self, b: u8) {
        if self.paste {
            self.paste_byte(b);
            return;
        }
        match self.state {
            State::Ground => self.ground(b),
            State::Esc => self.escape(b),
            State::Utf8 { len, alt } => self.utf8(b, len, alt),
            State::Csi => self.csi(b),
            State::Ss3 => self.ss3(b),
            State::Str { esc } => {
                self.state = if b == 0x07 || (esc && b == b'\\') {
                    State::Ground
                } else {
                    State::Str { esc: b == 0x1b }
                };
            }
        }
    }

    fn ground(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Esc,
            0xc2..=0xf4 => self.start_utf8(b, false),
            0x80..=0xff => {}
            _ => {
                let (mods, key) = legacy(b);
                self.key(mods, key);
            }
        }
    }

    fn escape(&mut self, b: u8) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.seq.clear();
                self.overflow = false;
                self.state = State::Csi;
            }
            b'O' => self.state = State::Ss3,
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::Str { esc: false },
            0x1b => {
                self.key(0, Key::Named("Esc"));
                self.state = State::Esc;
            }
            0xc2..=0xf4 => self.start_utf8(b, true),
            0x80..=0xc1 | 0xf5..=0xff => {}
            _ => {
                let (mods, key) = legacy(b);
                self.key(mods | MOD_ALT, key);
            }
        }
    }

    fn start_utf8(&mut self, b: u8, alt: bool) {
        self.seq.clear();
        self.seq.push(b);
        let len = match b {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        self.state = State::Utf8 { len, alt };
    }

    fn utf8(&mut self, b: u8, len: u8, alt: bool) {
        if b & 0xc0 != 0x80 {
            self.seq.clear();
            self.state = State::Ground;
            self.byte(b);
            return;
        }
        self.seq.push(b);
        if self.seq.len() < len as usize {
            return;
        }
        self.state = State::Ground;
        let ch = std::str::from_utf8(&self.seq)
            .ok()
            .and_then(|s| s.chars().next());
        self.seq.clear();
        if let Some(c) = ch {
            self.key(if alt { MOD_ALT } else { 0 }, Key::Char(c));
        }
    }

    fn ss3(&mut self, b: u8) {
        if b == 0x1b {
            self.state = State::Esc;
            return;
        }
        self.state = State::Ground;
        if let Some(key) = letter_key(b) {
            self.key(0, key);
        }
    }

    fn csi(&mut self, b: u8) {
        match b {
            0x40..=0x7e => {
                self.state = State::Ground;
                let body = std::mem::take(&mut self.seq);
                let overflow = std::mem::take(&mut self.overflow);
                if !overflow {
                    self.csi_dispatch(&body, b);
                }
            }
            0x20..=0x3f => {
                if self.seq.len() < MAX_SEQ {
                    self.seq.push(b);
                } else {
                    self.overflow = true;
                }
            }
            0x1b => {
                self.seq.clear();
                self.overflow = false;
                self.state = State::Esc;
            }
            0x00..=0x1f => {}
            _ => {
                self.seq.clear();
                self.overflow = false;
                self.state = State::Ground;
            }
        }
    }

    fn csi_dispatch(&mut self, body: &[u8], fin: u8) {
        let (private, body) = match body.first() {
            Some(&p) if (0x3c..=0x3f).contains(&p) => (Some(p), &body[1..]),
            _ => (None, body),
        };
        if body
            .iter()
            .any(|b| !(b.is_ascii_digit() || *b == b';' || *b == b':'))
        {
            return;
        }
        let params = parse_params(body);
        if private == Some(b'<') {
            if fin == b'M' || fin == b'm' {
                self.mouse(&params, fin == b'M');
            }
            return;
        }
        if private.is_some() || field(&params, 1, 1) == 3 {
            return;
        }
        let mods = mod_bits(&params);
        match fin {
            b'I' if body.is_empty() => self.push_event(InputEvent::FocusIn),
            b'O' if body.is_empty() => self.push_event(InputEvent::FocusOut),
            b'u' => self.kitty(&params, mods),
            b'~' => {
                let n = field(&params, 0, 0);
                if n == 200 {
                    self.start_paste();
                } else if let Some(key) = tilde_key(n) {
                    self.key(mods, key);
                }
            }
            b'Z' => self.key(mods | MOD_SHIFT, Key::Named("Tab")),
            _ => {
                if let Some(key) = letter_key(fin) {
                    self.key(mods, key);
                }
            }
        }
    }

    fn kitty(&mut self, params: &[Vec<u32>], mut mods: u8) {
        let code = field(params, 0, 0);
        let shifted = field(params, 0, 1);
        let text: Vec<char> = params
            .get(2)
            .map(|group| {
                group
                    .iter()
                    .filter_map(|&c| char::from_u32(c))
                    .filter(|&c| c != '\0')
                    .collect()
            })
            .unwrap_or_default();
        if mods & !MOD_SHIFT == 0 && !text.is_empty() {
            for c in text {
                self.key(0, Key::Char(c));
            }
            return;
        }
        let key = match code {
            27 => Key::Named("Esc"),
            9 => Key::Named("Tab"),
            13 | 57414 => Key::Named("CR"),
            127 => Key::Named("BS"),
            32 => Key::Char(' '),
            57399..=57408 => Key::Char(char::from(b'0' + (code - 57399) as u8)),
            57409 => Key::Char('.'),
            57410 => Key::Char('/'),
            57411 => Key::Char('*'),
            57412 => Key::Char('-'),
            57413 => Key::Char('+'),
            57415 => Key::Char('='),
            57416 => Key::Char(','),
            57417 => Key::Named("Left"),
            57418 => Key::Named("Right"),
            57419 => Key::Named("Up"),
            57420 => Key::Named("Down"),
            57421 => Key::Named("PageUp"),
            57422 => Key::Named("PageDown"),
            57423 => Key::Named("Home"),
            57424 => Key::Named("End"),
            57425 => Key::Named("Insert"),
            57426 => Key::Named("Del"),
            57364..=57398 => Key::F(code - 57363),
            57344..=63743 => return,
            _ => match char::from_u32(code) {
                Some(c) if code >= 32 => Key::Char(c),
                _ => return,
            },
        };
        let key = match key {
            Key::Char(c) if mods & MOD_SHIFT != 0 => {
                match char::from_u32(shifted).filter(|_| shifted != 0) {
                    Some(s) => {
                        mods &= !MOD_SHIFT;
                        Key::Char(s)
                    }
                    None if c.is_uppercase() => {
                        mods &= !MOD_SHIFT;
                        Key::Char(c)
                    }
                    None => Key::Char(c),
                }
            }
            other => other,
        };
        self.key(mods, key);
    }

    fn mouse(&mut self, params: &[Vec<u32>], pressed: bool) {
        if params.len() < 3 {
            return;
        }
        let b = field(params, 0, 0);
        let low = b & 3;
        let motion = b & 32 != 0;
        let (button, action) = if b & 64 != 0 {
            ("wheel", ["up", "down", "left", "right"][low as usize])
        } else if b & 128 != 0 {
            let name = if low & 1 == 0 { "x1" } else { "x2" };
            let action = if motion {
                "drag"
            } else if pressed {
                "press"
            } else {
                "release"
            };
            (name, action)
        } else {
            let name = match low {
                0 => "left",
                1 => "middle",
                2 => "right",
                _ => "",
            };
            match (name, motion, pressed) {
                ("", true, _) => ("move", ""),
                ("", false, _) => return,
                (name, true, _) => (name, "drag"),
                (name, false, true) => (name, "press"),
                (name, false, false) => (name, "release"),
            }
        };
        let mut modifier = String::new();
        if b & 16 != 0 {
            modifier.push_str("C-");
        }
        if b & 8 != 0 {
            modifier.push_str("A-");
        }
        if b & 4 != 0 {
            modifier.push_str("S-");
        }
        self.push_event(InputEvent::Mouse(Mouse {
            button,
            action,
            modifier,
            row: cell(field(params, 2, 0)),
            col: cell(field(params, 1, 0)),
        }));
    }

    fn start_paste(&mut self) {
        self.paste = true;
        self.paste_body.clear();
        self.paste_pending.clear();
        self.push_event(InputEvent::PasteStart);
    }

    fn paste_byte(&mut self, b: u8) {
        self.paste_pending.push(b);
        if self.paste_pending == PASTE_END {
            self.paste_pending.clear();
            self.paste = false;
            let body = std::mem::take(&mut self.paste_body);
            let text = convert_paste(&body);
            if !text.is_empty() {
                self.push_event(InputEvent::Paste(text));
            }
            self.push_event(InputEvent::PasteEnd);
            return;
        }
        while !PASTE_END.starts_with(&self.paste_pending) {
            let confirmed = self.paste_pending.remove(0);
            self.paste_body.push(confirmed);
        }
    }

    /// Emits the paste bytes that can be decided now; a trailing CR or an incomplete UTF-8
    /// character waits for the next read.
    fn flush_paste(&mut self) {
        if !self.paste {
            return;
        }
        let mut end = self.paste_body.len();
        if self.paste_body.last() == Some(&b'\r') {
            end -= 1;
        }
        let cut = incomplete_utf8_start(&self.paste_body[..end]);
        if cut == 0 {
            return;
        }
        let text = convert_paste(&self.paste_body[..cut]);
        self.paste_body.drain(..cut);
        if !text.is_empty() {
            self.push_event(InputEvent::Paste(text));
        }
    }

    fn key(&mut self, mods: u8, key: Key) {
        let s = encode(mods, key);
        self.keys.push_str(&s);
    }

    fn push_event(&mut self, event: InputEvent) {
        self.flush_keys();
        self.out.push(event);
    }

    fn flush_keys(&mut self) {
        if !self.keys.is_empty() {
            let keys = std::mem::take(&mut self.keys);
            self.out.push(InputEvent::Keys(keys));
        }
    }
}

#[derive(Clone, Copy)]
enum Key {
    Char(char),
    Named(&'static str),
    F(u32),
}

/// Neovim key notation for `mods` (MOD_* bits) and `key`.
fn encode(mods: u8, key: Key) -> String {
    match key {
        Key::Char(c) if mods == 0 => {
            if c == '<' {
                "<lt>".to_owned()
            } else {
                c.to_string()
            }
        }
        Key::Char(c) if mods == MOD_SHIFT => c.to_uppercase().collect(),
        _ => {
            let mut s = String::from("<");
            if mods & MOD_CTRL != 0 {
                s.push_str("C-");
            }
            if mods & MOD_ALT != 0 {
                s.push_str("M-");
            }
            if mods & MOD_SHIFT != 0 {
                s.push_str("S-");
            }
            if mods & MOD_SUPER != 0 {
                s.push_str("D-");
            }
            s.push_str(&key_name(key));
            s.push('>');
            s
        }
    }
}

fn key_name(key: Key) -> String {
    match key {
        Key::Char('<') => "lt".to_owned(),
        Key::Char(' ') => "Space".to_owned(),
        Key::Char('\\') => "Bslash".to_owned(),
        Key::Char('|') => "Bar".to_owned(),
        Key::Char(c) => c.to_string(),
        Key::Named(name) => name.to_owned(),
        Key::F(n) => format!("F{n}"),
    }
}

/// The key and modifiers a single byte below 0x80 stands for when it arrives as text.
fn legacy(b: u8) -> (u8, Key) {
    match b {
        0x00 => (MOD_CTRL, Key::Char(' ')),
        0x08 => (MOD_CTRL, Key::Char('h')),
        0x09 => (0, Key::Named("Tab")),
        0x0d => (0, Key::Named("CR")),
        0x01..=0x1a => (MOD_CTRL, Key::Char((b - 1 + b'a') as char)),
        0x1c..=0x1f => (MOD_CTRL, Key::Char((b + 0x40) as char)),
        0x7f => (0, Key::Named("BS")),
        _ => (0, Key::Char(b as char)),
    }
}

fn letter_key(fin: u8) -> Option<Key> {
    let name = match fin {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        b'P' => return Some(Key::F(1)),
        b'Q' => return Some(Key::F(2)),
        b'R' => return Some(Key::F(3)),
        b'S' => return Some(Key::F(4)),
        _ => return None,
    };
    Some(Key::Named(name))
}

fn tilde_key(n: u32) -> Option<Key> {
    Some(match n {
        1 | 7 => Key::Named("Home"),
        2 => Key::Named("Insert"),
        3 => Key::Named("Del"),
        4 | 8 => Key::Named("End"),
        5 => Key::Named("PageUp"),
        6 => Key::Named("PageDown"),
        11..=15 => Key::F(n - 10),
        17..=21 => Key::F(n - 11),
        23 | 24 => Key::F(n - 12),
        _ => return None,
    })
}

/// Splits `body` (digits, `;` and `:` only) into `;` groups of `:` fields. Absent fields are 0.
fn parse_params(body: &[u8]) -> Vec<Vec<u32>> {
    if body.is_empty() {
        return Vec::new();
    }
    body.split(|&b| b == b';')
        .map(|group| {
            group
                .split(|&b| b == b':')
                .map(|digits| {
                    digits.iter().fold(0u32, |n, &d| {
                        n.saturating_mul(10).saturating_add(u32::from(d - b'0'))
                    })
                })
                .collect()
        })
        .collect()
}

fn field(params: &[Vec<u32>], group: usize, sub: usize) -> u32 {
    params
        .get(group)
        .and_then(|g| g.get(sub))
        .copied()
        .unwrap_or(0)
}

/// Modifier bits of the `;mods` group (`1 + bits` on the wire), without lock bits.
fn mod_bits(params: &[Vec<u32>]) -> u8 {
    match field(params, 1, 0) {
        0 => 0,
        v => ((v - 1) & 0x0f) as u8,
    }
}

fn cell(v: u32) -> u16 {
    u16::try_from(v.saturating_sub(1)).unwrap_or(u16::MAX)
}

fn convert_paste(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Where an incomplete UTF-8 character at the end of `bytes` starts, or `bytes.len()`.
fn incomplete_utf8_start(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let i = n - back;
        let b = bytes[i];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let need = match b {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return n,
        };
        return if back < need { i } else { n };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> Vec<InputEvent> {
        let mut parser = InputParser::new();
        chunks.iter().flat_map(|c| parser.feed(c)).collect()
    }

    fn one(bytes: &[u8]) -> Vec<InputEvent> {
        InputParser::new().feed(bytes)
    }

    fn keys(s: &str) -> InputEvent {
        InputEvent::Keys(s.to_owned())
    }

    fn mouse(
        button: &'static str,
        action: &'static str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> InputEvent {
        InputEvent::Mouse(Mouse {
            button,
            action,
            modifier: modifier.to_owned(),
            row,
            col,
        })
    }

    #[test]
    fn plain_text_and_coalescing() {
        assert_eq!(one(b"ab"), vec![keys("ab")]);
        assert_eq!(one(b"ab\r"), vec![keys("ab<CR>")]);
        assert_eq!(one(b""), Vec::new());
        assert_eq!(one(b"a|\\ b"), vec![keys("a|\\ b")]);
    }

    #[test]
    fn utf8_split_across_feeds() {
        assert_eq!(feed_all(&["é".as_bytes()]), vec![keys("é")]);
        let euro = "€".as_bytes();
        let mut parser = InputParser::new();
        assert_eq!(parser.feed(&euro[..1]), Vec::new());
        assert_eq!(parser.feed(&euro[1..2]), Vec::new());
        assert_eq!(parser.feed(&euro[2..]), vec![keys("€")]);
    }

    #[test]
    fn invalid_utf8_is_dropped() {
        assert_eq!(one(b"\xff"), Vec::new());
        assert_eq!(one(b"\xc3("), vec![keys("(")]);
    }

    #[test]
    fn lt_is_escaped() {
        assert_eq!(one(b"<"), vec![keys("<lt>")]);
        assert_eq!(one(b"a<b"), vec![keys("a<lt>b")]);
    }

    #[test]
    fn control_keys() {
        assert_eq!(one(b"\x01"), vec![keys("<C-a>")]);
        assert_eq!(one(b"\x00"), vec![keys("<C-Space>")]);
        assert_eq!(one(b"\x08"), vec![keys("<C-h>")]);
        assert_eq!(one(b"\x0a"), vec![keys("<C-j>")]);
        assert_eq!(one(b"\x09"), vec![keys("<Tab>")]);
        assert_eq!(one(b"\x0d"), vec![keys("<CR>")]);
        assert_eq!(one(b"\x7f"), vec![keys("<BS>")]);
        assert_eq!(one(b"\x1a"), vec![keys("<C-z>")]);
        assert_eq!(one(b"\x1c"), vec![keys("<C-Bslash>")]);
        assert_eq!(one(b"\x1d"), vec![keys("<C-]>")]);
        assert_eq!(one(b"\x1e"), vec![keys("<C-^>")]);
        assert_eq!(one(b"\x1f"), vec![keys("<C-_>")]);
    }

    #[test]
    fn lone_escape_at_end_of_feed() {
        let mut parser = InputParser::new();
        assert_eq!(parser.feed(b"\x1b"), vec![keys("<Esc>")]);
        assert_eq!(parser.feed(b"x"), vec![keys("x")]);
    }

    #[test]
    fn legacy_alt() {
        assert_eq!(one(b"\x1bx"), vec![keys("<M-x>")]);
        assert_eq!(one(b"\x1b<"), vec![keys("<M-lt>")]);
        assert_eq!(one(b"\x1bA"), vec![keys("<M-A>")]);
        assert_eq!(one(b"\x1b\x7f"), vec![keys("<M-BS>")]);
        assert_eq!(one(b"\x1b\x01"), vec![keys("<C-M-a>")]);
        assert_eq!(one(b"\x1b "), vec![keys("<M-Space>")]);
    }

    #[test]
    fn csi_and_ss3_named_keys() {
        assert_eq!(one(b"\x1b[A"), vec![keys("<Up>")]);
        assert_eq!(one(b"\x1bOA"), vec![keys("<Up>")]);
        assert_eq!(one(b"\x1b[1;5A"), vec![keys("<C-Up>")]);
        assert_eq!(one(b"\x1b[1;2D"), vec![keys("<S-Left>")]);
        assert_eq!(one(b"\x1b[1;7C"), vec![keys("<C-M-Right>")]);
        assert_eq!(one(b"\x1b[H"), vec![keys("<Home>")]);
        assert_eq!(one(b"\x1b[F"), vec![keys("<End>")]);
        assert_eq!(one(b"\x1b[1~"), vec![keys("<Home>")]);
        assert_eq!(one(b"\x1b[4~"), vec![keys("<End>")]);
        assert_eq!(one(b"\x1b[7~"), vec![keys("<Home>")]);
        assert_eq!(one(b"\x1b[8~"), vec![keys("<End>")]);
        assert_eq!(one(b"\x1b[2~"), vec![keys("<Insert>")]);
        assert_eq!(one(b"\x1b[3~"), vec![keys("<Del>")]);
        assert_eq!(one(b"\x1b[5~"), vec![keys("<PageUp>")]);
        assert_eq!(one(b"\x1b[6;5~"), vec![keys("<C-PageDown>")]);
        assert_eq!(one(b"\x1b[Z"), vec![keys("<S-Tab>")]);
    }

    #[test]
    fn function_keys() {
        assert_eq!(one(b"\x1bOP"), vec![keys("<F1>")]);
        assert_eq!(one(b"\x1bOS"), vec![keys("<F4>")]);
        assert_eq!(one(b"\x1b[1;5P"), vec![keys("<C-F1>")]);
        assert_eq!(one(b"\x1b[13~"), vec![keys("<F3>")]);
        assert_eq!(one(b"\x1b[15~"), vec![keys("<F5>")]);
        assert_eq!(one(b"\x1b[15;2~"), vec![keys("<S-F5>")]);
        assert_eq!(one(b"\x1b[21~"), vec![keys("<F10>")]);
        assert_eq!(one(b"\x1b[24~"), vec![keys("<F12>")]);
    }

    #[test]
    fn unknown_sequences_are_dropped() {
        assert_eq!(one(b"\x1b[99;3~"), Vec::new());
        assert_eq!(one(b"\x1b[?62c"), Vec::new());
        assert_eq!(one(b"\x1b]0;title\x07"), Vec::new());
        assert_eq!(one(b"\x1bP1$r\x1b\\"), Vec::new());
        assert_eq!(one(b"\x1b[1 q"), Vec::new());
        assert_eq!(one(b"\x1b[1\x7f"), Vec::new());
        assert_eq!(one(b"\x1b[1q\x1b[A"), vec![keys("<Up>")]);
        assert_eq!(one(b"\x1b[\x1b[A"), vec![keys("<Up>")]);
        assert_eq!(one(b"\x1b[$zb"), vec![keys("b")]);
        assert_eq!(
            one(b"a\x1b[9999999999999999999999999999999999999999999999~b"),
            vec![keys("ab")]
        );
    }

    #[test]
    fn kitty_escape_and_ctrl_shift() {
        assert_eq!(one(b"\x1b[27u"), vec![keys("<Esc>")]);
        assert_eq!(one(b"\x1b[97;6u"), vec![keys("<C-S-a>")]);
        assert_eq!(one(b"\x1b[97;5u"), vec![keys("<C-a>")]);
        assert_eq!(one(b"\x1b[13u"), vec![keys("<CR>")]);
        assert_eq!(one(b"\x1b[127u"), vec![keys("<BS>")]);
        assert_eq!(one(b"\x1b[9;2u"), vec![keys("<S-Tab>")]);
        assert_eq!(one(b"\x1b[120;3u"), vec![keys("<M-x>")]);
        assert_eq!(one(b"\x1b[32;5u"), vec![keys("<C-Space>")]);
        assert_eq!(one(b"\x1b[57376u"), vec![keys("<F13>")]);
    }

    #[test]
    fn kitty_shift_folding() {
        assert_eq!(one(b"\x1b[97;2u"), vec![keys("A")]);
        assert_eq!(one(b"\x1b[97;2;65u"), vec![keys("A")]);
        assert_eq!(one(b"\x1b[49:33;4u"), vec![keys("<M-!>")]);
    }

    #[test]
    fn kitty_release_dropped_and_repeat_kept() {
        assert_eq!(one(b"\x1b[97;1:3u"), Vec::new());
        assert_eq!(one(b"\x1b[97;1:2u"), vec![keys("a")]);
        assert_eq!(one(b"\x1b[97;5:3u"), Vec::new());
        assert_eq!(one(b"\x1b[57441u"), Vec::new());
        assert_eq!(one(b"\x1b[57376;1:3u"), Vec::new());
    }

    #[test]
    fn kitty_text_field() {
        assert_eq!(one(b"\x1b[97;1;97u"), vec![keys("a")]);
        assert_eq!(one(b"\x1b[97;5;97u"), vec![keys("<C-a>")]);
        assert_eq!(one(b"\x1b[97;1;97:98u"), vec![keys("ab")]);
    }

    #[test]
    fn kitty_keypad() {
        assert_eq!(one(b"\x1b[57399u"), vec![keys("0")]);
        assert_eq!(one(b"\x1b[57401u"), vec![keys("2")]);
        assert_eq!(one(b"\x1b[57409u"), vec![keys(".")]);
        assert_eq!(one(b"\x1b[57413u"), vec![keys("+")]);
        assert_eq!(one(b"\x1b[57414u"), vec![keys("<CR>")]);
        assert_eq!(one(b"\x1b[57417u"), vec![keys("<Left>")]);
        assert_eq!(one(b"\x1b[57426;5u"), vec![keys("<C-Del>")]);
    }

    #[test]
    fn kitty_lock_bits_are_ignored() {
        assert_eq!(one(b"\x1b[97;65u"), vec![keys("a")]);
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(
            one(b"\x1b[200~hi\r\nthere\x1b[201~"),
            vec![
                InputEvent::PasteStart,
                InputEvent::Paste("hi\nthere".to_owned()),
                InputEvent::PasteEnd,
            ]
        );
    }

    #[test]
    fn paste_does_not_interpret_keys() {
        assert_eq!(
            one(b"\x1b[200~\x1b[A\r\x1b[201~"),
            vec![
                InputEvent::PasteStart,
                InputEvent::Paste("\x1b[A\n".to_owned()),
                InputEvent::PasteEnd,
            ]
        );
    }

    #[test]
    fn paste_end_marker_split_across_feeds() {
        let mut parser = InputParser::new();
        assert_eq!(
            parser.feed(b"\x1b[200~ab\x1b[20"),
            vec![InputEvent::PasteStart, InputEvent::Paste("ab".to_owned())]
        );
        assert_eq!(parser.feed(b"1~c"), vec![InputEvent::PasteEnd, keys("c")]);
    }

    #[test]
    fn paste_chunks_keep_crlf_and_utf8_whole() {
        let mut parser = InputParser::new();
        assert_eq!(
            parser.feed(b"\x1b[200~a\r"),
            vec![InputEvent::PasteStart, InputEvent::Paste("a".to_owned())]
        );
        assert_eq!(
            parser.feed(b"\nb\xe2\x82"),
            vec![InputEvent::Paste("\nb".to_owned())]
        );
        assert_eq!(
            parser.feed(b"\xac\x1b[201~"),
            vec![InputEvent::Paste("€".to_owned()), InputEvent::PasteEnd]
        );
    }

    #[test]
    fn sgr_mouse_press_release_drag_wheel_move() {
        assert_eq!(one(b"\x1b[<0;5;3M"), vec![mouse("left", "press", "", 2, 4)]);
        assert_eq!(
            one(b"\x1b[<0;5;3m"),
            vec![mouse("left", "release", "", 2, 4)]
        );
        assert_eq!(
            one(b"\x1b[<2;1;1M"),
            vec![mouse("right", "press", "", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<1;1;1M"),
            vec![mouse("middle", "press", "", 0, 0)]
        );
        assert_eq!(one(b"\x1b[<32;6;4M"), vec![mouse("left", "drag", "", 3, 5)]);
        assert_eq!(one(b"\x1b[<35;2;2M"), vec![mouse("move", "", "", 1, 1)]);
        assert_eq!(one(b"\x1b[<64;1;1M"), vec![mouse("wheel", "up", "", 0, 0)]);
        assert_eq!(
            one(b"\x1b[<65;1;1M"),
            vec![mouse("wheel", "down", "", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<66;1;1M"),
            vec![mouse("wheel", "left", "", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<67;1;1M"),
            vec![mouse("wheel", "right", "", 0, 0)]
        );
        assert_eq!(one(b"\x1b[<128;1;1M"), vec![mouse("x1", "press", "", 0, 0)]);
        assert_eq!(one(b"\x1b[<129;1;1M"), vec![mouse("x2", "press", "", 0, 0)]);
    }

    #[test]
    fn sgr_mouse_modifiers() {
        assert_eq!(
            one(b"\x1b[<20;1;1M"),
            vec![mouse("left", "press", "C-S-", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<10;1;1M"),
            vec![mouse("right", "press", "A-", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<28;1;1M"),
            vec![mouse("left", "press", "C-A-S-", 0, 0)]
        );
        assert_eq!(
            one(b"\x1b[<80;1;1M"),
            vec![mouse("wheel", "up", "C-", 0, 0)]
        );
    }

    #[test]
    fn sgr_mouse_out_of_range_coordinates_do_not_panic() {
        let _ = one(b"\x1b[<0;99999999999;0M");
    }

    #[test]
    fn focus_reports() {
        assert_eq!(one(b"\x1b[I"), vec![InputEvent::FocusIn]);
        assert_eq!(one(b"\x1b[O"), vec![InputEvent::FocusOut]);
    }

    #[test]
    fn events_keep_order_and_break_key_runs() {
        assert_eq!(
            one(b"ab\r\x1b[Ic\x1b[<0;1;1Mx"),
            vec![
                keys("ab<CR>"),
                InputEvent::FocusIn,
                keys("c"),
                mouse("left", "press", "", 0, 0),
                keys("x"),
            ]
        );
    }

    #[test]
    fn sequence_split_across_feeds() {
        let mut parser = InputParser::new();
        assert_eq!(parser.feed(b"\x1b[1;"), Vec::new());
        assert_eq!(parser.feed(b"5A"), vec![keys("<C-Up>")]);
        assert_eq!(parser.feed(b"\x1b"), vec![keys("<Esc>")]);
    }

    #[test]
    fn random_bytes_never_panic() {
        const POOL: &[u8] = b"\x1b[]O;:0123456789<>?~uIMmPZ\x7f\r\n\t\x07\\200201";
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let mut parser = InputParser::new();
            for _ in 0..20 {
                let len = (next() % 40) as usize;
                let chunk: Vec<u8> = (0..len)
                    .map(|_| {
                        let r = next();
                        if r & 1 == 0 {
                            POOL[(r >> 1) as usize % POOL.len()]
                        } else {
                            (r >> 8) as u8
                        }
                    })
                    .collect();
                for event in parser.feed(&chunk) {
                    if let InputEvent::Keys(s) = event {
                        assert!(!s.is_empty());
                    }
                }
            }
        }
    }
}
