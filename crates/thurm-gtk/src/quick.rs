//! The quick terminal (QuickTerminal.swift): a single-tab window shown and hidden by a global
//! hotkey. The hotkey goes through the XDG GlobalShortcuts portal where the desktop has one;
//! `thurm-gtk --quick-terminal` toggles it too (bind it in the desktop's keyboard settings).
//! Wayland does not let apps place windows, so it fades in where the compositor puts it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use thurm_config::{QuickTerminalPosition, QuickTerminalScreen};

use crate::app::{self, App, leaves};
use crate::config::UiConfig;
use crate::core::{LOCAL, PaneKey};
use crate::model::SplitNode;
use crate::tab::Tab;

pub struct QuickTerminal {
    window: adw::Window,
    pub tab: Rc<Tab>,
    shown: Cell<bool>,
    generation: Cell<u64>,
}

impl QuickTerminal {
    fn new(app: &Rc<App>, tab: Rc<Tab>) -> Rc<QuickTerminal> {
        let window = adw::Window::new();
        window.set_application(Some(&app.gtk_app));
        window.set_decorated(false);
        window.set_title(Some("Quick Terminal"));
        window.add_css_class("thurm");
        window.add_css_class("thurm-quick");
        window.set_content(Some(&tab.root));
        let q = Rc::new(QuickTerminal {
            window,
            tab,
            shown: Cell::new(false),
            generation: Cell::new(0),
        });
        q.apply_config(&app.ui());
        q.window.connect_is_active_notify(|w| {
            if w.is_active() {
                return;
            }
            // Autohide when another window takes the focus (dialogs and the palette stay).
            glib::idle_add_local_once(|| {
                app::with_app(|a| {
                    let q = a.quick.borrow().clone();
                    if let Some(q) = q
                        && q.shown.get()
                        && !q.window.is_active()
                        && a.ui().cfg.quick_terminal.autohide
                    {
                        q.hide();
                    }
                });
            });
        });
        q.window.connect_close_request(|w| {
            w.set_visible(false);
            app::with_app(|a| {
                if let Some(q) = a.quick.borrow().as_ref() {
                    q.shown.set(false);
                }
            });
            glib::Propagation::Stop
        });
        q
    }

    pub fn apply_config(&self, ui: &UiConfig) {
        let qt = &ui.cfg.quick_terminal;
        let size = qt.size.clamp(0.1, 1.0);
        let display = gtk::gdk::Display::default();
        let monitor = display.and_then(|d| {
            let monitors = d.monitors();
            let index = match qt.screen {
                QuickTerminalScreen::Main => 0,
                _ => 0,
            };
            monitors.item(index).and_downcast::<gtk::gdk::Monitor>()
        });
        let (mw, mh) = monitor
            .map(|m| {
                let g = m.geometry();
                (g.width(), g.height())
            })
            .unwrap_or((1440, 900));
        let (w, h) = match qt.position {
            QuickTerminalPosition::Top | QuickTerminalPosition::Bottom => {
                (mw, (mh as f64 * size) as i32)
            }
            QuickTerminalPosition::Left | QuickTerminalPosition::Right => {
                ((mw as f64 * size) as i32, mh)
            }
            _ => ((mw as f64 * size) as i32, (mh as f64 * size) as i32),
        };
        self.window.set_default_size(w, h);
    }

    pub fn is_active(&self) -> bool {
        self.shown.get() && self.window.is_active()
    }

    pub fn show(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let duration = app::with_app(|a| a.ui().cfg.quick_terminal.animation_duration.clamp(0.0, 5.0))
            .unwrap_or(0.2);
        self.shown.set(true);
        if !self.window.is_visible() {
            self.window.set_opacity(if duration > 0.0 { 0.0 } else { 1.0 });
        }
        self.window.present();
        let k = self.tab.focused.borrow().clone();
        app::with_app(|a| {
            if let Some(v) = a.view(&k) {
                v.grab_focus();
            }
        });
        if duration > 0.0 {
            fade(&self.window, 1.0, duration, None);
        }
    }

    pub fn hide(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.shown.set(false);
        let duration = app::with_app(|a| a.ui().cfg.quick_terminal.animation_duration.clamp(0.0, 5.0))
            .unwrap_or(0.2);
        let window = self.window.clone();
        let done = move || {
            let still = app::with_app(|a| {
                a.quick
                    .borrow()
                    .as_ref()
                    .is_some_and(|q| q.generation.get() == generation)
            })
            .unwrap_or(false);
            if still {
                window.set_visible(false);
                window.set_opacity(1.0);
            }
        };
        if duration > 0.0 {
            fade(&self.window, 0.0, duration, Some(Box::new(done)));
        } else {
            done();
        }
    }

    /// Its last pane ended: the next show starts a new shell.
    pub fn closed(&self) {
        self.window.set_visible(false);
        self.shown.set(false);
        let w = self.window.clone();
        glib::idle_add_local_once(move || {
            w.destroy();
        });
        app::with_app(|a| {
            a.quick.borrow_mut().take();
            a.quick_layout.borrow_mut().take();
            a.schedule_save();
        });
    }
}

/// Animates the window's opacity to `to` (ease-out).
fn fade(window: &adw::Window, to: f64, secs: f64, done: Option<Box<dyn FnOnce()>>) {
    let from = window.opacity();
    let target = adw::CallbackAnimationTarget::new({
        let w = window.downgrade();
        move |v| {
            if let Some(w) = w.upgrade() {
                w.set_opacity(v);
            }
        }
    });
    let anim = adw::TimedAnimation::new(window, from, to, (secs * 1000.0) as u32, target);
    anim.set_easing(adw::Easing::EaseOutCubic);
    if let Some(done) = done {
        let done = RefCell::new(Some(done));
        anim.connect_done(move |_| {
            if let Some(f) = done.borrow_mut().take() {
                f();
            }
        });
    }
    anim.play();
}

pub fn toggle(app: &Rc<App>) {
    let existing = app.quick.borrow().clone();
    match existing {
        Some(q) if q.is_active() => q.hide(),
        Some(q) => q.show(),
        None => {
            let Some(tab) = quick_tab(app) else { return };
            let q = QuickTerminal::new(app, tab.clone());
            *app.quick.borrow_mut() = Some(q.clone());
            tab.sync_children();
            q.show();
            app.schedule_save();
        }
    }
}

/// The stored quick tab when its panes still run, else a new shell.
fn quick_tab(app: &Rc<App>) -> Option<Rc<Tab>> {
    let stored = app.quick_layout.borrow_mut().take();
    if let Some(l) = stored {
        let alive = leaves(&l.root)
            .iter()
            .all(|(_, id)| app.infos.borrow().contains_key(&PaneKey::local(*id)));
        if alive && let Some(t) = Tab::from_layout(&l, LOCAL, 0) {
            return Some(t);
        }
    }
    let key = app.create_pane(LOCAL, None, None, None, None, false)?;
    Some(Tab::new(SplitNode::Leaf(key.clone()), key, 0))
}

// MARK: global hotkey

thread_local! {
    static BOUND: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn configure(app: &App) {
    let spec = app.ui().cfg.quick_terminal.hotkey.trim().to_string();
    if spec.is_empty() {
        return;
    }
    match portal_trigger(&spec) {
        Some(trigger) => {
            BOUND.with(|b| *b.borrow_mut() = spec.clone());
            bind_portal(trigger, spec);
        }
        None => app.toast(&format!("Quick terminal: unknown hotkey \"{spec}\""), 8.0),
    }
}

pub fn configure_changed(app: &App) {
    let spec = app.ui().cfg.quick_terminal.hotkey.trim().to_string();
    if BOUND.with(|b| *b.borrow() != spec) {
        configure(app);
    }
    if let Some(q) = app.quick.borrow().as_ref() {
        q.apply_config(&app.ui());
    }
}

/// `ctrl+grave` → `CTRL+grave` (the shortcuts portal's trigger format).
pub fn portal_trigger(spec: &str) -> Option<String> {
    let parts: Vec<String> = spec.split('+').map(|p| p.trim().to_lowercase()).collect();
    let (key, mods) = parts.split_last()?;
    let mut out = Vec::new();
    for m in mods {
        out.push(match m.as_str() {
            "cmd" | "command" | "super" => "LOGO",
            "ctrl" | "control" => "CTRL",
            "alt" | "opt" | "option" => "ALT",
            "shift" => "SHIFT",
            _ => return None,
        });
    }
    let key = match key.as_str() {
        "grave" | "`" | "backtick" => "grave".to_string(),
        "minus" | "-" => "minus".into(),
        "equal" | "=" => "equal".into(),
        "left_bracket" | "[" => "bracketleft".into(),
        "right_bracket" | "]" => "bracketright".into(),
        "backslash" | "\\" => "backslash".into(),
        "semicolon" | ";" => "semicolon".into(),
        "quote" | "'" => "apostrophe".into(),
        "comma" | "," => "comma".into(),
        "period" | "." => "period".into(),
        "slash" | "/" => "slash".into(),
        "space" => "space".into(),
        "tab" => "Tab".into(),
        "return" | "enter" => "Return".into(),
        "escape" | "esc" => "Escape".into(),
        "backspace" => "BackSpace".into(),
        k if k.len() > 1 && k.starts_with('f') && k[1..].parse::<u8>().is_ok_and(|n| (1..=20).contains(&n)) => {
            k.to_uppercase()
        }
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => k.to_string(),
        _ => return None,
    };
    let mut s = out.join("+");
    if !s.is_empty() {
        s.push('+');
    }
    s.push_str(&key);
    Some(s)
}

/// CreateSession → BindShortcuts → Activated, on org.freedesktop.portal.GlobalShortcuts.
#[allow(deprecated)]
fn bind_portal(trigger: String, spec: String) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return;
    };
    let token = format!("thurm{}", std::process::id());
    let opts = glib::VariantDict::new(None);
    opts.insert_value("handle_token", &token.to_variant());
    opts.insert_value("session_handle_token", &token.to_variant());
    let reply = bus.call_sync(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
        "CreateSession",
        Some(&(opts.end(),).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        3000,
        None::<&gio::Cancellable>,
    );
    let Ok(reply) = reply else {
        log::info!("no GlobalShortcuts portal; bind `thurm-gtk --quick-terminal` in the desktop's keyboard settings for {spec}");
        return;
    };
    let request: String = reply.child_value(0).get::<String>().unwrap_or_default();
    let bus2 = bus.clone();
    let sub = Rc::new(RefCell::new(None));
    let sub2 = sub.clone();
    let id = bus.signal_subscribe(
        Some("org.freedesktop.portal.Desktop"),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&request),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if let Some(id) = sub2.borrow_mut().take() {
                bus2.signal_unsubscribe(id);
            }
            let results = params.child_value(1);
            let dict = glib::VariantDict::new(Some(&results));
            let Some(session) = dict
                .lookup_value("session_handle", None)
                .and_then(|v| v.str().map(str::to_string))
            else {
                return;
            };
            bind_shortcut(&bus2, &session, &trigger);
        },
    );
    *sub.borrow_mut() = Some(id);
    // Toggle on activation.
    bus.signal_subscribe(
        Some("org.freedesktop.portal.Desktop"),
        Some("org.freedesktop.portal.GlobalShortcuts"),
        Some("Activated"),
        Some("/org/freedesktop/portal/desktop"),
        None,
        gio::DBusSignalFlags::NONE,
        |_, _, _, _, _, params| {
            if params.child_value(1).str() == Some("quick-terminal") {
                app::with_app(|a| a.toggle_quick());
            }
        },
    );
}

fn bind_shortcut(bus: &gio::DBusConnection, session: &str, trigger: &str) {
    let props = glib::VariantDict::new(None);
    props.insert_value("description", &"Show or hide the quick terminal".to_variant());
    props.insert_value("preferred_trigger", &trigger.to_variant());
    let shortcuts = vec![("quick-terminal".to_string(), props.end())];
    let opts = glib::VariantDict::new(None);
    let Ok(path) = glib::variant::ObjectPath::try_from(session.to_string()) else { return };
    let params = (path, shortcuts, String::new(), opts.end()).to_variant();
    let _ = bus.call_sync(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
        "BindShortcuts",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        3000,
        None::<&gio::Cancellable>,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(portal_trigger("ctrl+grave").as_deref(), Some("CTRL+grave"));
        assert_eq!(portal_trigger("cmd+shift+space").as_deref(), Some("LOGO+SHIFT+space"));
        assert_eq!(portal_trigger("f12").as_deref(), Some("F12"));
        assert_eq!(portal_trigger("hyper+x"), None);
    }
}
