//! The terminal's drawing area, exposing the screen's text to screen readers through
//! `GtkAccessibleText` (TerminalAccessibility.swift): the visible rows, the caret at the
//! cursor, and character, word and line navigation.

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TermArea {
        /// The visible screen, rows joined with '\n'.
        pub text: RefCell<String>,
        /// Caret, in characters.
        pub caret: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TermArea {
        const NAME: &'static str = "ThurmTermArea";
        type Type = super::TermArea;
        type ParentType = gtk::DrawingArea;
        type Interfaces = (gtk::AccessibleText,);
    }

    impl ObjectImpl for TermArea {}
    impl WidgetImpl for TermArea {}
    impl DrawingAreaImpl for TermArea {}

    impl AccessibleTextImpl for TermArea {
        fn caret_position(&self) -> u32 {
            self.caret.get()
        }

        fn contents(&self, start: u32, end: u32) -> Option<glib::Bytes> {
            let text = self.text.borrow();
            let s: String = text
                .chars()
                .skip(start as usize)
                .take(end.saturating_sub(start) as usize)
                .collect();
            Some(glib::Bytes::from_owned(s.into_bytes()))
        }

        fn contents_at(
            &self,
            offset: u32,
            granularity: gtk::AccessibleTextGranularity,
        ) -> Option<(u32, u32, glib::Bytes)> {
            let chars: Vec<char> = self.text.borrow().chars().collect();
            let (start, end) = super::unit_at(&chars, offset as usize, granularity);
            let s: String = chars[start..end].iter().collect();
            Some((start as u32, end as u32, glib::Bytes::from_owned(s.into_bytes())))
        }

        fn selection(&self) -> Vec<gtk::AccessibleTextRange> {
            Vec::new()
        }

        fn attributes(&self, _offset: u32) -> Vec<(gtk::AccessibleTextRange, glib::GString, glib::GString)> {
            Vec::new()
        }

        fn default_attributes(&self) -> Vec<(glib::GString, glib::GString)> {
            Vec::new()
        }
    }
}

glib::wrapper! {
    pub struct TermArea(ObjectSubclass<imp::TermArea>)
        @extends gtk::DrawingArea, gtk::Widget,
        @implements gtk::Accessible, gtk::AccessibleText, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TermArea {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl TermArea {
    /// Replaces the screen text and caret, telling assistive technologies what changed.
    pub fn set_text(&self, text: String, caret: u32) {
        let imp = self.imp();
        let old_len = imp.text.borrow().chars().count() as u32;
        let changed = *imp.text.borrow() != text;
        if changed {
            let new_len = text.chars().count() as u32;
            *imp.text.borrow_mut() = text;
            self.update_contents(gtk::AccessibleTextContentChange::Remove, 0, old_len);
            self.update_contents(gtk::AccessibleTextContentChange::Insert, 0, new_len);
        }
        if changed || imp.caret.get() != caret {
            imp.caret.set(caret);
            self.update_caret_position();
        }
    }
}

/// The character, word or line around `offset` as a [start, end) char range.
fn unit_at(chars: &[char], offset: usize, granularity: gtk::AccessibleTextGranularity) -> (usize, usize) {
    let n = chars.len();
    let offset = offset.min(n);
    match granularity {
        gtk::AccessibleTextGranularity::Character => (offset, (offset + 1).min(n)),
        gtk::AccessibleTextGranularity::Word => {
            // The run of word characters, or of separators, the offset is in (at the end: the
            // last one); a line break is a unit of its own.
            if n == 0 {
                return (0, 0);
            }
            let class = |c: char| match c {
                '\n' => 2,
                c if c.is_alphanumeric() || c == '_' => 0,
                _ => 1,
            };
            let at = offset.min(n - 1);
            let k = class(chars[at]);
            if k == 2 {
                return (at, at + 1);
            }
            let mut s = at;
            while s > 0 && class(chars[s - 1]) == k {
                s -= 1;
            }
            let mut e = at + 1;
            while e < n && class(chars[e]) == k {
                e += 1;
            }
            (s, e)
        }
        _ => {
            let mut s = offset;
            while s > 0 && chars[s - 1] != '\n' {
                s -= 1;
            }
            let mut e = offset;
            while e < n && chars[e] != '\n' {
                e += 1;
            }
            (s, e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        let chars: Vec<char> = "ls -la\nhello world".chars().collect();
        assert_eq!(unit_at(&chars, 8, gtk::AccessibleTextGranularity::Line), (7, 18));
        assert_eq!(unit_at(&chars, 14, gtk::AccessibleTextGranularity::Word), (13, 18));
        // Separators are units too, so word navigation can step over them.
        assert_eq!(unit_at(&chars, 2, gtk::AccessibleTextGranularity::Word), (2, 4));
        assert_eq!(unit_at(&chars, 12, gtk::AccessibleTextGranularity::Word), (12, 13));
        assert_eq!(unit_at(&chars, 6, gtk::AccessibleTextGranularity::Word), (6, 7));
        assert_eq!(unit_at(&chars, 18, gtk::AccessibleTextGranularity::Word), (13, 18));
        assert_eq!(unit_at(&chars, 0, gtk::AccessibleTextGranularity::Character), (0, 1));
    }
}
