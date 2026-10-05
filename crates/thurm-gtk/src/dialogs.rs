//! Modal questions (NSAlert in the macOS app) as libadwaita alert dialogs.

use adw::prelude::*;
use gtk::glib;

/// One button of a dialog: response id, label, appearance.
pub struct Button<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub style: Style,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Default,
    Suggested,
    Destructive,
}

pub fn button<'a>(id: &'a str, label: &'a str, style: Style) -> Button<'a> {
    Button { id, label, style }
}

/// Asks with `buttons`; `done` gets the chosen response id ("cancel" when dismissed).
pub fn ask(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    body: &str,
    buttons: &[Button<'_>],
    done: impl FnOnce(String) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(title), Some(body));
    for b in buttons {
        dialog.add_response(b.id, b.label);
        match b.style {
            Style::Suggested => dialog.set_response_appearance(b.id, adw::ResponseAppearance::Suggested),
            Style::Destructive => {
                dialog.set_response_appearance(b.id, adw::ResponseAppearance::Destructive)
            }
            Style::Default => {}
        }
    }
    if let Some(first) = buttons.first() {
        dialog.set_default_response(Some(first.id));
    }
    if buttons.iter().any(|b| b.id == "cancel") {
        dialog.set_close_response("cancel");
    }
    let done = std::cell::RefCell::new(Some(done));
    dialog.connect_response(None, move |_, id| {
        if let Some(f) = done.borrow_mut().take() {
            f(id.to_string());
        }
    });
    dialog.present(Some(parent));
}

/// `ok_label` / Cancel; `done` runs only when confirmed.
pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    body: &str,
    ok_label: &str,
    destructive: bool,
    done: impl FnOnce() + 'static,
) {
    let style = if destructive { Style::Destructive } else { Style::Suggested };
    ask(
        parent,
        title,
        body,
        &[button("ok", ok_label, style), button("cancel", "Cancel", Style::Default)],
        move |r| {
            if r == "ok" {
                done();
            }
        },
    );
}

/// An informational message with OK.
pub fn inform(parent: &impl IsA<gtk::Widget>, title: &str, body: &str) {
    ask(parent, title, body, &[button("ok", "OK", Style::Default)], |_| {});
}

/// A dialog with text fields; `done` gets the values when confirmed.
pub fn prompt(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    body: &str,
    fields: &[(&str, &str, &str)],
    ok_label: &str,
    done: impl FnOnce(Vec<String>) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(title), (!body.is_empty()).then_some(body));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let mut entries = Vec::new();
    for (label, placeholder, value) in fields {
        if !label.is_empty() {
            let l = gtk::Label::new(Some(label));
            l.set_xalign(0.0);
            l.add_css_class("dim-label");
            list.append(&l);
        }
        let e = gtk::Entry::new();
        e.set_placeholder_text(Some(placeholder));
        e.set_text(value);
        e.set_activates_default(true);
        e.set_width_chars(32);
        list.append(&e);
        entries.push(e);
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", ok_label);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let done = std::cell::RefCell::new(Some(done));
    dialog.connect_response(None, move |_, id| {
        if id == "ok"
            && let Some(f) = done.borrow_mut().take()
        {
            f(entries.iter().map(|e| e.text().to_string()).collect());
        }
    });
    dialog.present(Some(parent));
    if let Some(first) = list.first_child().and_then(|c| {
        if c.is::<gtk::Entry>() { Some(c) } else { c.next_sibling() }
    }) {
        glib::idle_add_local_once(move || {
            first.grab_focus();
        });
    }
}
