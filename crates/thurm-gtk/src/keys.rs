//! GDK key events to Thurm key codes (thurm.h). The Linux counterpart of KeyMapping.swift.

use gtk::gdk;
use gtk::gdk::Key;
use gtk::gdk::prelude::DisplayExtManual;
use gtk::glib::translate::IntoGlib;
pub const KIND_TEXT: u32 = 0;
pub const KIND_NAMED: u32 = 1;

pub const PRESS: u8 = 0;
pub const REPEAT: u8 = 1;
pub const RELEASE: u8 = 2;

pub const MOD_SHIFT: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_CTRL: u8 = 4;
pub const MOD_SUPER: u8 = 8;
pub const MOD_CAPS_LOCK: u8 = 64;

const ESCAPE: u32 = 1;
const ENTER: u32 = 2;
const TAB: u32 = 3;
const BACKSPACE: u32 = 4;
const INSERT: u32 = 5;
const DELETE: u32 = 6;
const LEFT: u32 = 7;
const RIGHT: u32 = 8;
const UP: u32 = 9;
const DOWN: u32 = 10;
const PAGE_UP: u32 = 11;
const PAGE_DOWN: u32 = 12;
const HOME: u32 = 13;
const END: u32 = 14;
const CAPS_LOCK: u32 = 15;
const SCROLL_LOCK: u32 = 16;
const NUM_LOCK: u32 = 17;
const PRINT_SCREEN: u32 = 18;
const PAUSE: u32 = 19;
const MENU: u32 = 20;
const KP_0: u32 = 21;
const KP_DECIMAL: u32 = 31;
const KP_DIVIDE: u32 = 32;
const KP_MULTIPLY: u32 = 33;
const KP_SUBTRACT: u32 = 34;
const KP_ADD: u32 = 35;
const KP_ENTER: u32 = 36;
const KP_EQUAL: u32 = 37;
const LEFT_SHIFT: u32 = 38;
const LEFT_CONTROL: u32 = 39;
const LEFT_ALT: u32 = 40;
const LEFT_SUPER: u32 = 41;
const RIGHT_SHIFT: u32 = 42;
const RIGHT_CONTROL: u32 = 43;
const RIGHT_ALT: u32 = 44;
const RIGHT_SUPER: u32 = 45;
const VOLUME_DOWN: u32 = 46;
const VOLUME_UP: u32 = 47;
const VOLUME_MUTE: u32 = 48;
const F1: u32 = 100;

/// The named (non-text) key for `key`, if it is one. Keypad keys report whether they are.
pub fn named(key: Key) -> Option<(u32, bool)> {
    let raw = key.into_glib();
    // F1..F35 are consecutive keysyms.
    if (Key::F1.into_glib()..=Key::F35.into_glib()).contains(&raw) {
        return Some((F1 + raw - Key::F1.into_glib(), false));
    }
    if (Key::KP_0.into_glib()..=Key::KP_9.into_glib()).contains(&raw) {
        return Some((KP_0 + raw - Key::KP_0.into_glib(), true));
    }
    let named = match key {
        Key::Escape => ESCAPE,
        Key::Return => ENTER,
        Key::Tab | Key::ISO_Left_Tab => TAB,
        Key::BackSpace => BACKSPACE,
        Key::Insert => INSERT,
        Key::Delete => DELETE,
        Key::Left => LEFT,
        Key::Right => RIGHT,
        Key::Up => UP,
        Key::Down => DOWN,
        Key::Page_Up => PAGE_UP,
        Key::Page_Down => PAGE_DOWN,
        Key::Home => HOME,
        Key::End => END,
        Key::Caps_Lock => CAPS_LOCK,
        Key::Scroll_Lock => SCROLL_LOCK,
        Key::Num_Lock => NUM_LOCK,
        Key::Print => PRINT_SCREEN,
        Key::Pause => PAUSE,
        Key::Menu => MENU,
        Key::Shift_L => LEFT_SHIFT,
        Key::Shift_R => RIGHT_SHIFT,
        Key::Control_L => LEFT_CONTROL,
        Key::Control_R => RIGHT_CONTROL,
        Key::Alt_L | Key::Meta_L => LEFT_ALT,
        Key::Alt_R | Key::Meta_R | Key::ISO_Level3_Shift => RIGHT_ALT,
        Key::Super_L => LEFT_SUPER,
        Key::Super_R => RIGHT_SUPER,
        Key::AudioLowerVolume => VOLUME_DOWN,
        Key::AudioRaiseVolume => VOLUME_UP,
        Key::AudioMute => VOLUME_MUTE,
        _ => {
            let kp = match key {
                Key::KP_Decimal | Key::KP_Separator => KP_DECIMAL,
                Key::KP_Divide => KP_DIVIDE,
                Key::KP_Multiply => KP_MULTIPLY,
                Key::KP_Subtract => KP_SUBTRACT,
                Key::KP_Add => KP_ADD,
                Key::KP_Enter => KP_ENTER,
                Key::KP_Equal => KP_EQUAL,
                // NumLock off: the keypad sends navigation keysyms.
                Key::KP_Home => HOME,
                Key::KP_End => END,
                Key::KP_Up => UP,
                Key::KP_Down => DOWN,
                Key::KP_Left => LEFT,
                Key::KP_Right => RIGHT,
                Key::KP_Page_Up => PAGE_UP,
                Key::KP_Page_Down => PAGE_DOWN,
                Key::KP_Insert => INSERT,
                Key::KP_Delete => DELETE,
                _ => return None,
            };
            return Some((kp, true));
        }
    };
    Some((named, false))
}

/// Kitty-protocol modifier bits.
pub fn mods(state: gdk::ModifierType) -> u8 {
    let mut m = 0;
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        m |= MOD_SHIFT;
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        m |= MOD_CTRL;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        m |= MOD_ALT;
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        m |= MOD_SUPER;
    }
    if state.contains(gdk::ModifierType::LOCK_MASK) {
        m |= MOD_CAPS_LOCK;
    }
    m
}

/// Text a terminal should receive as typed (no control characters).
pub fn is_printable(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c >= ' ' && c != '\u{7f}')
}

/// The key's character with the given modifier state applied.
fn translated(
    display: &gdk::Display,
    keycode: u32,
    state: gdk::ModifierType,
    group: i32,
) -> Option<char> {
    let (keyval, ..) = display.translate_key(keycode, state, group)?;
    keyval.to_unicode().filter(|c| !c.is_control())
}

/// `code` (the unshifted character, lowercased), `shifted` and `base_layout` (the US-layout
/// key, from the first group) for a text key.
pub fn text_codes(display: &gdk::Display, keycode: u32, group: i32) -> (u32, u32, u32) {
    let none = gdk::ModifierType::empty();
    let code = translated(display, keycode, none, group)
        .map(|c| c.to_lowercase().next().unwrap_or(c) as u32)
        .unwrap_or(0);
    let shifted = translated(display, keycode, gdk::ModifierType::SHIFT_MASK, group)
        .map(|c| c as u32)
        .unwrap_or(0);
    let base = if group != 0 {
        translated(display, keycode, none, 0).map_or(0, |c| c as u32)
    } else {
        0
    };
    (
        code,
        if shifted == code { 0 } else { shifted },
        if base == code { 0 } else { base },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys() {
        assert_eq!(named(Key::Return), Some((ENTER, false)));
        assert_eq!(named(Key::ISO_Left_Tab), Some((TAB, false)));
        assert_eq!(named(Key::F1), Some((F1, false)));
        assert_eq!(named(Key::F12), Some((F1 + 11, false)));
        assert_eq!(named(Key::KP_7), Some((KP_0 + 7, true)));
        assert_eq!(named(Key::KP_Enter), Some((KP_ENTER, true)));
        assert_eq!(named(Key::KP_Home), Some((HOME, true)));
        assert_eq!(named(Key::a), None);
    }

    #[test]
    fn modifier_bits() {
        let s = gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK;
        assert_eq!(mods(s), MOD_SHIFT | MOD_CTRL | MOD_ALT);
        assert_eq!(mods(gdk::ModifierType::SUPER_MASK), MOD_SUPER);
    }

    #[test]
    fn printable() {
        assert!(is_printable("a"));
        assert!(is_printable("日本"));
        assert!(!is_printable("\u{1b}"));
        assert!(!is_printable(""));
    }
}
