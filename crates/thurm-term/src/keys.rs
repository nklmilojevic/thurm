//! Key, mouse, focus and paste encoding, by libghostty-vt's encoders (legacy xterm sequences
//! and the kitty keyboard protocol, configured from the terminal's modes). Thurm maps its
//! events onto libghostty-vt's.

use std::ptr;

use libghostty_vt_sys as ffi;
use thurm_proto::{Key, KeyAction, KeyEvent, MouseButton, MouseEvent, MouseKind, NamedKey, mods};

use crate::vt::Vt;

/// libghostty-vt's key and mouse encoders, with an event of each kind to fill.
pub struct Encoders {
    key: ffi::KeyEncoder,
    key_event: ffi::KeyEvent,
    mouse: ffi::MouseEncoder,
    mouse_event: ffi::MouseEvent,
}

// Only used behind `&mut` of the terminal that owns them.
unsafe impl Send for Encoders {}

impl Default for Encoders {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Encoders {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_key_event_free(self.key_event);
            ffi::ghostty_key_encoder_free(self.key);
            ffi::ghostty_mouse_event_free(self.mouse_event);
            ffi::ghostty_mouse_encoder_free(self.mouse);
        }
    }
}

/// Where the grid is, for mouse positions: its size in cells and a cell's size in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

impl Encoders {
    pub fn new() -> Self {
        let mut e = Self {
            key: ptr::null_mut(),
            key_event: ptr::null_mut(),
            mouse: ptr::null_mut(),
            mouse_event: ptr::null_mut(),
        };
        unsafe {
            ffi::ghostty_key_encoder_new(ptr::null(), &mut e.key);
            ffi::ghostty_key_event_new(ptr::null(), &mut e.key_event);
            ffi::ghostty_mouse_encoder_new(ptr::null(), &mut e.mouse);
            ffi::ghostty_mouse_event_new(ptr::null(), &mut e.mouse_event);
        }
        e
    }

    /// Bytes for the PTY for a key, in the terminal's current modes. `kitty`: the kitty
    /// keyboard protocol is enabled in the config (off: keys stay legacy whatever the program
    /// pushed).
    pub fn key(&mut self, vt: &Vt, ev: &KeyEvent, kitty: bool) -> Vec<u8> {
        // libghostty-vt's keys stop at F25; the kitty protocol numbers F26-F35 on.
        if let Key::Named(NamedKey::F(n @ 26..=35)) = ev.key {
            return kitty_function_key(n, ev, if kitty { vt.kitty_flags() } else { 0 });
        }
        let (key, unshifted) = match ev.key {
            Key::Char(c) => (physical_key(ev.base_layout.unwrap_or(c)), c as u32),
            Key::Named(k) => (named_key(k), 0),
        };
        let m = ghostty_mods(ev.mods);
        // Shift is used up producing a shifted character ("A", "!"); the encoder reports it
        // only where the protocol wants it.
        let consumed = match ev.key {
            Key::Char(c)
                if ev.mods & mods::SHIFT != 0
                    && !ev.text.is_empty()
                    && ev.text != c.to_string() =>
            {
                ffi::MODS_SHIFT
            }
            _ => 0,
        };
        // The key's text as the encoder wants it: the character itself where the app sends
        // none or a control character (Ctrl combinations).
        let text = match ev.key {
            Key::Char(c) if ev.text.is_empty() || ev.text.chars().all(char::is_control) => {
                c.to_string()
            }
            _ => ev.text.clone(),
        };
        let action = match ev.action {
            KeyAction::Press => ffi::KeyAction::PRESS,
            KeyAction::Repeat => ffi::KeyAction::REPEAT,
            KeyAction::Release => ffi::KeyAction::RELEASE,
        };
        let mut out = unsafe {
            ffi::ghostty_key_encoder_setopt_from_terminal(self.key, vt.raw());
            if !kitty {
                let none: ffi::KittyKeyFlags = 0;
                ffi::ghostty_key_encoder_setopt(
                    self.key,
                    ffi::KeyEncoderOption::KITTY_FLAGS,
                    (&none as *const ffi::KittyKeyFlags).cast(),
                );
            }
            // Option-as-Alt is decided by the app: ALT only arrives when it applies.
            let alt = ffi::OptionAsAlt::TRUE;
            ffi::ghostty_key_encoder_setopt(
                self.key,
                ffi::KeyEncoderOption::MACOS_OPTION_AS_ALT,
                (&alt as *const ffi::OptionAsAlt::Type).cast(),
            );
            let ev_ = self.key_event;
            ffi::ghostty_key_event_set_action(ev_, action);
            ffi::ghostty_key_event_set_key(ev_, key);
            ffi::ghostty_key_event_set_mods(ev_, m);
            ffi::ghostty_key_event_set_consumed_mods(ev_, consumed);
            ffi::ghostty_key_event_set_unshifted_codepoint(ev_, unshifted);
            ffi::ghostty_key_event_set_utf8(ev_, text.as_ptr().cast(), text.len());
            encode(|buf, len, out| ffi::ghostty_key_encoder_encode(self.key, ev_, buf, len, out))
        };
        // Line feed/new line mode (LNM): Return sends CR LF.
        if out == b"\r" && vt.mode(crate::vt::mode(20, true)) {
            out.push(b'\n');
        }
        out
    }

    /// Mouse press/release/motion for an application that grabbed the mouse, if its mode
    /// reports this event.
    pub fn mouse(&mut self, vt: &Vt, ev: &MouseEvent, grid: Grid) -> Option<Vec<u8>> {
        let action = match ev.kind {
            MouseKind::Press => ffi::MouseAction::PRESS,
            MouseKind::Release => ffi::MouseAction::RELEASE,
            MouseKind::Move => ffi::MouseAction::MOTION,
        };
        let button = match ev.button {
            MouseButton::Left => Some(ffi::MouseButton::LEFT),
            MouseButton::Middle => Some(ffi::MouseButton::MIDDLE),
            MouseButton::Right => Some(ffi::MouseButton::RIGHT),
            MouseButton::Back => Some(ffi::MouseButton::EIGHT),
            MouseButton::Forward => Some(ffi::MouseButton::NINE),
            MouseButton::None => None,
        };
        // Within the cell the app put the pointer in (the pixel position may be anywhere).
        let (cw, ch) = (grid.cell_width.max(1), grid.cell_height.max(1));
        let x = ev.col as u32 * cw + ev.x.saturating_sub(ev.col as u32 * cw).min(cw - 1);
        let y = ev.row as u32 * ch + ev.y.saturating_sub(ev.row as u32 * ch).min(ch - 1);
        self.encode_mouse(
            vt,
            action,
            button,
            ev.mods,
            (x, y),
            grid,
            ev.button != MouseButton::None,
        )
    }

    /// Wheel movement (`lines` > 0: up) at a cell. For an application that grabbed the mouse
    /// this is buttons 4 / 5; on the alternate screen with alternate scroll mode (1007), arrow
    /// keys. `None` when neither applies.
    #[allow(clippy::too_many_arguments)]
    pub fn wheel(
        &mut self,
        vt: &Vt,
        lines: i32,
        col: u16,
        row: u16,
        m: u8,
        grid: Grid,
        kitty: bool,
    ) -> Option<Vec<u8>> {
        if lines == 0 {
            return None;
        }
        let tracking = [9, 1000, 1002, 1003]
            .iter()
            .any(|&n| vt.mode(crate::vt::mode(n, false)));
        let mut out = Vec::new();
        if tracking {
            let button = if lines > 0 {
                ffi::MouseButton::FOUR
            } else {
                ffi::MouseButton::FIVE
            };
            let pos = (
                col as u32 * grid.cell_width.max(1),
                row as u32 * grid.cell_height.max(1),
            );
            for _ in 0..lines.unsigned_abs() {
                out.extend(self.encode_mouse(
                    vt,
                    ffi::MouseAction::PRESS,
                    Some(button),
                    m,
                    pos,
                    grid,
                    false,
                )?);
            }
            return Some(out);
        }
        let alternate_scroll = vt.is_alt_screen() && vt.mode(crate::vt::mode(1007, false));
        if !alternate_scroll {
            return None;
        }
        let arrow = KeyEvent {
            key: Key::Named(if lines > 0 {
                NamedKey::Up
            } else {
                NamedKey::Down
            }),
            mods: 0,
            action: KeyAction::Press,
            text: String::new(),
            shifted: None,
            base_layout: None,
        };
        for _ in 0..lines.unsigned_abs() {
            out.extend(self.key(vt, &arrow, kitty));
        }
        Some(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_mouse(
        &mut self,
        vt: &Vt,
        action: ffi::MouseAction::Type,
        button: Option<ffi::MouseButton::Type>,
        m: u8,
        (x, y): (u32, u32),
        grid: Grid,
        pressed: bool,
    ) -> Option<Vec<u8>> {
        let size = ffi::MouseEncoderSize {
            size: std::mem::size_of::<ffi::MouseEncoderSize>(),
            screen_width: grid.cols as u32 * grid.cell_width.max(1),
            screen_height: grid.rows as u32 * grid.cell_height.max(1),
            cell_width: grid.cell_width.max(1),
            cell_height: grid.cell_height.max(1),
            ..Default::default()
        };
        let out = unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(self.mouse, vt.raw());
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse,
                ffi::MouseEncoderOption::SIZE,
                (&size as *const ffi::MouseEncoderSize).cast(),
            );
            let any = pressed && action == ffi::MouseAction::MOTION;
            ffi::ghostty_mouse_encoder_setopt(
                self.mouse,
                ffi::MouseEncoderOption::ANY_BUTTON_PRESSED,
                (&any as *const bool).cast(),
            );
            let ev = self.mouse_event;
            ffi::ghostty_mouse_event_set_action(ev, action);
            match button {
                Some(b) => ffi::ghostty_mouse_event_set_button(ev, b),
                None => ffi::ghostty_mouse_event_clear_button(ev),
            }
            ffi::ghostty_mouse_event_set_mods(ev, ghostty_mods(m));
            ffi::ghostty_mouse_event_set_position(
                ev,
                ffi::MousePosition {
                    x: x as f32,
                    y: y as f32,
                },
            );
            encode(|buf, len, out| ffi::ghostty_mouse_encoder_encode(self.mouse, ev, buf, len, out))
        };
        (!out.is_empty()).then_some(out)
    }
}

/// Focus in / out report (for mode 1004, which the caller checks).
pub fn focus(focused: bool) -> Vec<u8> {
    let ev = if focused {
        ffi::FocusEvent::GAINED
    } else {
        ffi::FocusEvent::LOST
    };
    unsafe { encode(|buf, len, out| ffi::ghostty_focus_encode(ev, buf, len, out)) }
}

/// Pasted text for the PTY: unsafe control bytes replaced, bracketed when the mode is on
/// (otherwise newlines become carriage returns).
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut data = text.as_bytes().to_vec();
    unsafe {
        encode(|buf, len, out| {
            ffi::ghostty_paste_encode(
                data.as_mut_ptr().cast(),
                data.len(),
                bracketed,
                buf,
                len,
                out,
            )
        })
    }
}

/// Whether pasting `text` is safe without asking (no newline, no bracketed-paste end marker).
pub fn paste_is_safe(text: &str) -> bool {
    unsafe { ffi::ghostty_paste_is_safe(text.as_ptr().cast(), text.len()) }
}

/// Runs a libghostty-vt encode call, growing the buffer when it's too small.
unsafe fn encode(
    mut f: impl FnMut(*mut std::ffi::c_char, usize, *mut usize) -> ffi::Result::Type,
) -> Vec<u8> {
    let mut buf = vec![0u8; 64];
    let mut len = 0usize;
    let mut r = f(buf.as_mut_ptr().cast(), buf.len(), &mut len);
    if r == ffi::Result::OUT_OF_SPACE {
        buf.resize(len, 0);
        r = f(buf.as_mut_ptr().cast(), buf.len(), &mut len);
    }
    if r != ffi::Result::SUCCESS {
        return Vec::new();
    }
    buf.truncate(len);
    buf
}

/// F26-F35 in the kitty keyboard protocol (`CSI code ; mods u`, codes 57389-57398): only
/// with the protocol on (legacy encodings have no sequence for them), presses and repeats,
/// and releases when the program asked for event types.
fn kitty_function_key(n: u8, ev: &KeyEvent, flags: u8) -> Vec<u8> {
    // No legacy form exists: any kitty flag makes them CSI-u.
    if flags == 0 || (ev.action == KeyAction::Release && flags & 2 == 0) {
        return Vec::new();
    }
    let code = 57363 + u32::from(n);
    // The protocol's modifier bits are Thurm's own (shift 1, alt 2, ctrl 4, super 8, hyper
    // 16, meta 32, caps lock 64, num lock 128).
    let mods = u32::from(ev.mods) + 1;
    let event = match ev.action {
        KeyAction::Press => "",
        KeyAction::Repeat if flags & 2 != 0 => ":2",
        KeyAction::Repeat => "",
        KeyAction::Release => ":3",
    };
    if mods == 1 && event.is_empty() {
        format!("\x1b[{code}u").into_bytes()
    } else {
        format!("\x1b[{code};{mods}{event}u").into_bytes()
    }
}

fn ghostty_mods(m: u8) -> ffi::Mods {
    let mut g = 0;
    for (t, f) in [
        (mods::SHIFT, ffi::MODS_SHIFT),
        (mods::CTRL, ffi::MODS_CTRL),
        (mods::ALT, ffi::MODS_ALT),
        (mods::SUPER, ffi::MODS_SUPER),
        (mods::CAPS_LOCK, ffi::MODS_CAPS_LOCK),
        (mods::NUM_LOCK, ffi::MODS_NUM_LOCK),
    ] {
        if m & t != 0 {
            g |= f;
        }
    }
    g
}

/// The US-layout physical key producing `c` unshifted.
fn physical_key(c: char) -> ffi::Key::Type {
    use ffi::Key as K;
    match c {
        'a'..='z' => K::A + (c as u32 - 'a' as u32) as ffi::Key::Type,
        '0'..='9' => K::DIGIT_0 + (c as u32 - '0' as u32) as ffi::Key::Type,
        '`' => K::BACKQUOTE,
        '\\' => K::BACKSLASH,
        '[' => K::BRACKET_LEFT,
        ']' => K::BRACKET_RIGHT,
        ',' => K::COMMA,
        '=' => K::EQUAL,
        '-' => K::MINUS,
        '.' => K::PERIOD,
        '\'' => K::QUOTE,
        ';' => K::SEMICOLON,
        '/' => K::SLASH,
        ' ' => K::SPACE,
        _ => K::UNIDENTIFIED,
    }
}

fn named_key(k: NamedKey) -> ffi::Key::Type {
    use NamedKey as N;
    use ffi::Key as K;
    match k {
        N::Escape => K::ESCAPE,
        N::Enter => K::ENTER,
        N::Tab => K::TAB,
        N::Backspace => K::BACKSPACE,
        N::Insert => K::INSERT,
        N::Delete => K::DELETE,
        N::Left => K::ARROW_LEFT,
        N::Right => K::ARROW_RIGHT,
        N::Up => K::ARROW_UP,
        N::Down => K::ARROW_DOWN,
        N::PageUp => K::PAGE_UP,
        N::PageDown => K::PAGE_DOWN,
        N::Home => K::HOME,
        N::End => K::END,
        N::CapsLock => K::CAPS_LOCK,
        N::ScrollLock => K::SCROLL_LOCK,
        N::NumLock => K::NUM_LOCK,
        N::PrintScreen => K::PRINT_SCREEN,
        N::Pause => K::PAUSE,
        N::Menu => K::CONTEXT_MENU,
        N::F(n @ 1..=25) => K::F1 + (n as ffi::Key::Type - 1),
        N::F(_) => K::UNIDENTIFIED,
        N::Kp0 => K::NUMPAD_0,
        N::Kp1 => K::NUMPAD_1,
        N::Kp2 => K::NUMPAD_2,
        N::Kp3 => K::NUMPAD_3,
        N::Kp4 => K::NUMPAD_4,
        N::Kp5 => K::NUMPAD_5,
        N::Kp6 => K::NUMPAD_6,
        N::Kp7 => K::NUMPAD_7,
        N::Kp8 => K::NUMPAD_8,
        N::Kp9 => K::NUMPAD_9,
        N::KpDecimal => K::NUMPAD_DECIMAL,
        N::KpDivide => K::NUMPAD_DIVIDE,
        N::KpMultiply => K::NUMPAD_MULTIPLY,
        N::KpSubtract => K::NUMPAD_SUBTRACT,
        N::KpAdd => K::NUMPAD_ADD,
        N::KpEnter => K::NUMPAD_ENTER,
        N::KpEqual => K::NUMPAD_EQUAL,
        N::KpSeparator => K::NUMPAD_SEPARATOR,
        N::KpLeft => K::NUMPAD_LEFT,
        N::KpRight => K::NUMPAD_RIGHT,
        N::KpUp => K::NUMPAD_UP,
        N::KpDown => K::NUMPAD_DOWN,
        N::KpPageUp => K::NUMPAD_PAGE_UP,
        N::KpPageDown => K::NUMPAD_PAGE_DOWN,
        N::KpHome => K::NUMPAD_HOME,
        N::KpEnd => K::NUMPAD_END,
        N::KpInsert => K::NUMPAD_INSERT,
        N::KpDelete => K::NUMPAD_DELETE,
        N::KpBegin => K::NUMPAD_BEGIN,
        N::MediaPlay | N::MediaPause | N::MediaPlayPause => K::MEDIA_PLAY_PAUSE,
        N::MediaStop => K::MEDIA_STOP,
        N::MediaNext => K::MEDIA_TRACK_NEXT,
        N::MediaPrev => K::MEDIA_TRACK_PREVIOUS,
        N::VolumeDown => K::AUDIO_VOLUME_DOWN,
        N::VolumeUp => K::AUDIO_VOLUME_UP,
        N::VolumeMute => K::AUDIO_VOLUME_MUTE,
        N::LeftShift => K::SHIFT_LEFT,
        N::LeftControl => K::CONTROL_LEFT,
        N::LeftAlt => K::ALT_LEFT,
        N::LeftSuper => K::META_LEFT,
        N::RightShift => K::SHIFT_RIGHT,
        N::RightControl => K::CONTROL_RIGHT,
        N::RightAlt => K::ALT_RIGHT,
        N::RightSuper => K::META_RIGHT,
    }
}
