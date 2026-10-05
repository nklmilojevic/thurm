//! Desktop notifications (Notifications.swift) and the count of agents waiting for input, which
//! macOS shows on the Dock icon and Linux docks read from the Unity LauncherEntry API.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use serde_json::{Value, json};
use thurm_proto::AgentStatus;

use crate::app::{self, App};
use crate::core::PaneKey;

fn permission_id(key: &PaneKey) -> String {
    format!("permission-{}-{}", key.host, key.id)
}

pub fn install(app: &Rc<App>) {
    let focus = gio::SimpleAction::new("focus-pane", Some(glib::VariantTy::new("(st)").unwrap()));
    focus.connect_activate(|_, p| {
        if let Some((host, id)) = p.and_then(|v| v.get::<(String, u64)>()) {
            app::with_app(|a| a.focus_agent(&PaneKey::new(&host, id)));
        }
    });
    app.gtk_app.add_action(&focus);
    let answer = gio::SimpleAction::new(
        "answer-permission",
        Some(glib::VariantTy::new("(sttb)").unwrap()),
    );
    answer.connect_activate(|_, p| {
        let Some((host, id, prompt, allow)) = p.and_then(|v| v.get::<(String, u64, u64, bool)>())
        else {
            return;
        };
        app::with_app(|a| {
            let key = PaneKey::new(&host, id);
            let ok = a
                .core(&host)
                .map(|c| c.request(&json!({"AnswerPermission": {"pane": id, "prompt": prompt, "allow": allow}})))
                .is_some_and(|r| r == Value::String("Ok".into()));
            // Already answered (or not connected): show the pane instead.
            if !ok {
                a.focus_agent(&key);
            }
        });
    });
    app.gtk_app.add_action(&answer);
}

/// A `Notify` event: posted unless the pane is in front of the user.
pub fn post(app: &App, key: &PaneKey, payload: &Value) {
    if !app.ui().cfg.notifications.enabled || app.is_pane_focused(key) {
        return;
    }
    let mut title = payload
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or("Thurm")
        .to_string();
    if key.is_remote() {
        title = format!("{} · {title}", key.host);
    }
    let body = payload.get("body").and_then(Value::as_str).unwrap_or("");
    let n = gio::Notification::new(&title);
    n.set_body(Some(body));
    n.set_default_action_and_target_value(
        "app.focus-pane",
        Some(&(key.host.clone(), key.id).to_variant()),
    );
    match payload.get("permission").and_then(Value::as_u64) {
        Some(prompt) => {
            n.set_priority(gio::NotificationPriority::High);
            n.add_button_with_target_value(
                "Approve",
                "app.answer-permission",
                Some(&(key.host.clone(), key.id, prompt, true).to_variant()),
            );
            n.add_button_with_target_value(
                "Deny",
                "app.answer-permission",
                Some(&(key.host.clone(), key.id, prompt, false).to_variant()),
            );
            app.gtk_app.send_notification(Some(&permission_id(key)), &n);
        }
        None => app.gtk_app.send_notification(None, &n),
    }
}

/// Removes the pane's permission notification (answered, closed, or moved on).
pub fn withdraw_permission(app: &App, key: &PaneKey) {
    app.gtk_app.withdraw_notification(&permission_id(key));
}

/// Agents waiting for input that the user has not seen yet; a pane counts as seen once it is
/// on screen in the active window while waiting.
pub fn update_badge(app: &App) {
    let waiting: Vec<PaneKey> = app
        .infos
        .borrow()
        .iter()
        .filter(|(_, i)| i.agent.as_ref().is_some_and(|a| a.status == AgentStatus::NeedsInput))
        .map(|(k, _)| k.clone())
        .collect();
    let active = app.win().is_some_and(|w| w.window.is_active());
    let mut seen = app.seen_waiting();
    if active && let Some(tab) = app.current_tab() {
        let zoomed = tab.zoomed.borrow().clone();
        for k in tab.panes() {
            if zoomed.as_ref().is_none_or(|z| *z == k) && waiting.contains(&k) {
                seen.insert(k);
            }
        }
    }
    seen.retain(|k| waiting.contains(k));
    let count = waiting.iter().filter(|k| !seen.contains(k)).count();
    drop(seen);
    launcher_count(count);
}

fn launcher_count(count: usize) {
    thread_local! {
        static LAST: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }
    if LAST.with(|l| l.replace(Some(count))) == Some(count) {
        return;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return;
    };
    let props = glib::VariantDict::new(None);
    props.insert_value("count", &(count as i64).to_variant());
    props.insert_value("count-visible", &(count > 0).to_variant());
    let params = (
        format!("application://{}.desktop", crate::app::APP_ID),
        props.end(),
    )
        .to_variant();
    let _ = bus.emit_signal(
        None,
        "/rs/thurm/Thurm",
        "com.canonical.Unity.LauncherEntry",
        "Update",
        Some(&params),
    );
}
