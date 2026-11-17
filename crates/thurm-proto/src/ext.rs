//! The extension API: how plugins, scripts and apps add actions, hear events and ask the user
//! things.
//!
//! It is one set of JSON methods, the same over every transport:
//!
//! - a Lua plugin calls them in the daemon's process (`thurm.api(method, params)`),
//! - a process plugin over JSON-RPC on its standard input and output,
//! - a socket client with [`crate::Request::Ext`], answered with [`crate::Response::Text`]
//!   holding the result as JSON; what the daemon has to say on its own comes as
//!   [`crate::Event::Ext`].
//!
//! Who may use it: the daemon's socket is its own user's alone (mode 0600, in a private
//! directory), so a client there is that user, as the `thurm` CLI is, and may run
//! actions and programs like the CLI may run panes. Its plugins are that user's code. A tailnet
//! caller may only read `actions.list` and `plugins.list`; plugins' questions go only to the
//! app this daemon belongs to.
//!
//! JSON rather than postcard: a method or a field can be added without touching the wire
//! format, so an app and a daemon of different versions keep working (an unknown method is an
//! error, an unknown field is ignored, a missing one takes its default).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Methods a caller asks the daemon for.
pub mod method {
    /// `{request}`: any [`crate::Request`] as JSON (`"ListPanes"`, `{"ClosePane":{"pane":1}}`);
    /// the result is its [`crate::Response`] as JSON. Requests about the connection itself
    /// (`Hello`, `Subscribe`...) are refused for plugins.
    pub const REQUEST: &str = "request";
    /// Every action and key binding: [`super::Actions`].
    pub const ACTIONS_LIST: &str = "actions.list";
    /// `{actions: [ActionInfo]}`: the caller provides these actions from now on, and is asked
    /// to carry them out with [`super::notice::ACTION_INVOKE`]. Registering a name again
    /// replaces it; a name another plugin provides is refused.
    pub const ACTIONS_REGISTER: &str = "actions.register";
    /// `{names}`: the caller no longer provides these.
    pub const ACTIONS_UNREGISTER: &str = "actions.unregister";
    /// [`super::RunAction`]; the result is what the action returned.
    pub const ACTIONS_RUN: &str = "actions.run";
    /// [`super::Reply`] to an [`super::notice::ACTION_INVOKE`], from a socket client.
    pub const ACTIONS_RESULT: &str = "actions.result";
    /// [`super::Binding`]: `key` runs `action`. The caller's earlier binding of the key goes.
    pub const KEYS_BIND: &str = "keys.bind";
    /// `{key}`: drops the caller's binding of the key.
    pub const KEYS_UNBIND: &str = "keys.unbind";
    /// `{names}`: the caller hears these events ([`super::notice::EVENT`]); `"*"` is all.
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// `{names}`: stops them; no names stops all.
    pub const EVENTS_UNSUBSCRIBE: &str = "events.unsubscribe";
    /// `{name, payload}`: an event of the caller's own, heard by whoever subscribed to it. The
    /// name has a dot (`git.branch_changed`), which the daemon's own never have.
    pub const EVENTS_EMIT: &str = "events.emit";
    /// [`super::OpenPane`]: a new pane, shown where asked. The result is `{pane}`.
    pub const PANES_OPEN: &str = "panes.open";
    /// `{text, pane?}`: a short message over a pane (the focused one).
    pub const UI_TOAST: &str = "ui.toast";
    /// [`super::Pick`]: the user picks from a list. The result is `{index, id}` (`index` from
    /// 0) or null when the user cancels.
    pub const UI_PICK: &str = "ui.pick";
    /// [`super::Input`]: the user types a line. The result is the text or null.
    pub const UI_INPUT: &str = "ui.input";
    /// [`super::Confirm`]: the result is true or false.
    pub const UI_CONFIRM: &str = "ui.confirm";
    /// [`super::Reply`] to a [`super::notice::UI_REQUEST`], from the app that showed it.
    pub const UI_REPLY: &str = "ui.reply";
    /// [`super::Exec`]: runs a program to its end. The result is [`super::ExecResult`].
    pub const EXEC: &str = "exec";
    /// `{level, message}`: a line for the caller's log.
    pub const LOG: &str = "log";
    /// Every plugin found: `[PluginInfo]`.
    pub const PLUGINS_LIST: &str = "plugins.list";
    /// `{name?}`: stops and starts one plugin again, or looks for plugins anew and restarts all.
    pub const PLUGINS_RELOAD: &str = "plugins.reload";
    /// `{name}`: takes the plugin off `plugins.disabled` in the config and starts it.
    pub const PLUGINS_ENABLE: &str = "plugins.enable";
    /// `{name}`: stops the plugin and adds it to `plugins.disabled`.
    pub const PLUGINS_DISABLE: &str = "plugins.disable";
    /// `{name, lines?}`: the end of the plugin's log, as `{lines}`.
    pub const PLUGINS_LOGS: &str = "plugins.logs";
    /// [`super::RunScript`]: runs a Lua script to its end. Its log lines come as
    /// [`super::notice::SCRIPT_LOG`] to a socket caller.
    pub const SCRIPT_RUN: &str = "script.run";
    /// `{pane?}`: the TCP ports the programs in this daemon's panes (or in `pane`) listen on,
    /// as `[PanePorts]` ([`crate::PanePorts`]).
    pub const PORTS_LIST: &str = "ports.list";
    /// `{host, pane, ports, complete, initial?}`: a remote host's pane listed these ports
    /// ([`crate::Event::PortsChanged`] from that host's daemon); `initial` when that daemon
    /// marked it so (sent as the app joined its watch), or the app sends it again to this daemon
    /// on connecting. The result is `{heard}`: whether plugins heard it (only the lists of the
    /// app that holds plugins' forwards are passed on). `{host, gone: true}`: the app's
    /// connection to the host is gone (answered `{heard}` the same way). The daemon keeps each pane's latest list for new
    /// subscribers and for an app taking over. Only from the app
    /// ([`crate::CAP_EXT_FORWARDS`]), which hears them; subscribers hear `ports_changed` with
    /// `host`.
    pub const PORTS_REPORT: &str = "ports.report";
    /// [`super::ForwardsReport`]: the app's forwards now, and what changed. Only from the app
    /// ([`crate::CAP_EXT_FORWARDS`]); subscribers hear `forward_opened` and `forward_closed`.
    pub const FORWARDS_REPORT: &str = "forwards.report";
    /// The forwards of the app that holds plugins' forwards (the one that connected first; ids
    /// are that app's own): `[ForwardInfo]`. Fails when no app that forwards is connected.
    pub const FORWARDS_LIST: &str = "forwards.list";
    /// [`super::ForwardOpen`]: the app opens a remote host's port on this Mac (or finds the
    /// forward that has it). The result is a [`super::ForwardInfo`] with one more field, `new`:
    /// true when the forward was opened now.
    pub const FORWARDS_OPEN: &str = "forwards.open";
    /// `{id}`: the app closes that forward. The result is its [`super::ForwardInfo`].
    pub const FORWARDS_CLOSE: &str = "forwards.close";
}

/// What the daemon sends on its own: the `name` of an [`crate::Event::Ext`], or the method of
/// a JSON-RPC message to a process plugin.
pub mod notice {
    /// The actions or key bindings changed: ask for [`super::method::ACTIONS_LIST`] again.
    pub const ACTIONS_CHANGED: &str = "actions.changed";
    /// [`super::Invoke`]: carry out an action the receiver registered. A socket client answers
    /// with [`super::method::ACTIONS_RESULT`], a process plugin with the JSON-RPC result.
    pub const ACTION_INVOKE: &str = "action.invoke";
    /// `{name, ...}`: an event the receiver subscribed to.
    pub const EVENT: &str = "event";
    /// [`super::UiRequest`]: show it and answer with [`super::method::UI_REPLY`].
    pub const UI_REQUEST: &str = "ui.request";
    /// `{id}`: a [`UI_REQUEST`] nobody waits for any more; take it down.
    pub const UI_CANCEL: &str = "ui.cancel";
    /// `{run, level, message}`: a line a script run by the receiver logged.
    pub const SCRIPT_LOG: &str = "script.log";
    /// The plugins changed (started, stopped, failed).
    pub const PLUGINS_CHANGED: &str = "plugins.changed";
}

/// Events of the daemon's own that [`method::EVENTS_SUBSCRIBE`] takes. Each payload has
/// `event` (the name). The agent events, `command_finished`, `pane_exited`, `pane_created`,
/// `pane_changed` and `pane_focused` have the pane's [`crate::PaneInfo`] in `pane`;
/// `pane_closed`, `bell` and `notification` have only its id, in `id`.
///
/// - `ports_changed`: `{id, ports, complete}` ([`crate::PanePorts`]), the whole list of a pane's
///   ports (empty once they went), with `pane` while the pane is there. A remote host's pane
///   (reported by the app) has `host`, and no `pane`, and `initial` for the lists its daemon
///   sent the app as it joined that daemon's watch. Subscribing to it by name starts the
///   daemon's watch of its panes' ports. A new subscriber gets every pane's list first, empty
///   ones too, on its own (the others hear nothing again), and the remote hosts' lists known so
///   far (but panes plugins already heard listen on nothing), all with `initial`. When the app loses a host (or quits), each of its panes heard
///   of gets an empty list, `complete` false, with `gone`.
/// - `files_uploaded`: `{id, paths}`, the files sent to a pane, once their paths were typed
///   there, with `pane` while it is there.
/// - `forward_opened` and `forward_closed`: [`ForwardInfo`] (its `id` is the forward's, not a
///   pane's), as the app that holds plugins'
///   forwards reports them (the one that connected first; when it quits, the next app's are
///   heard as opened);
///   `forward_closed` with `why`: `closed` (by someone), `host` (removed or disabled), `gone`
///   (the port went away) or `app` (the app quit).
pub const EVENTS: &[&str] = &[
    "agent_working",
    "agent_waiting",
    "agent_blocked",
    "agent_finished",
    "agent_failed",
    "agent_idle",
    "command_finished",
    "pane_exited",
    "pane_created",
    "pane_closed",
    "pane_changed",
    "pane_focused",
    "bell",
    "notification",
    "config_reloaded",
    "ports_changed",
    "files_uploaded",
    "forward_opened",
    "forward_closed",
];

/// An action: something with a name that the command palette lists, a key can run, and the
/// CLI and scripts can call.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ActionInfo {
    /// How it is called: `git.sync`. Letters, digits, `.`, `_`, `-` and spaces.
    pub name: String,
    /// Shown in the palette and the menu; the name when empty.
    pub title: String,
    pub description: String,
    /// Groups it in the menu and prefixes it in the palette ("Git").
    pub category: String,
    /// Who provides it: `config`, `plugin:<name>`, `app`.
    pub source: String,
    /// Left out of the palette and the menu (a function bound to a key).
    pub hidden: bool,
    /// What it takes, as a JSON Schema object; null when it takes nothing or does not say.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub args: Value,
}

impl ActionInfo {
    pub fn title(&self) -> &str {
        if self.title.is_empty() {
            &self.name
        } else {
            &self.title
        }
    }
}

/// A key that runs an action, written like a `[keybindings]` chord (`cmd+shift+l`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Binding {
    pub key: String,
    pub action: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub args: Value,
    /// Who bound it, as [`ActionInfo::source`].
    pub source: String,
}

/// The answer to [`method::ACTIONS_LIST`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Actions {
    pub actions: Vec<ActionInfo>,
    pub bindings: Vec<Binding>,
}

/// Parameters of [`method::ACTIONS_RUN`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct RunAction {
    pub name: String,
    /// The pane it is for: the focused one, when a person asked.
    pub pane: Option<crate::PaneId>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub args: Value,
    /// How many actions deep the caller runs: an action that runs another passes its
    /// [`Context::depth`].
    pub depth: u32,
    /// For no pane at all: without `pane`, the daemon otherwise takes the focused one (an app
    /// whose focus is on another host's pane says this).
    pub no_pane: bool,
    /// The [`Invoke::id`] of the action the caller is carrying out, when it runs this one from
    /// it: the depth is that one's. Without it, a provider is taken to be as deep as the
    /// deepest action it has been asked to carry out.
    pub invocation: Option<u64>,
    /// With `no_pane`: the `[[remote]]` host whose pane the person is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The text selected in the pane, when an app runs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    /// The file the mouse pointer is over in the pane, as written there (a `file://` link as
    /// its path), when an app runs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// What an action's provider gets with [`notice::ACTION_INVOKE`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Invoke {
    /// Echoed in the [`Reply`] of a socket client.
    pub id: u64,
    pub name: String,
    pub args: Value,
    pub ctx: Context,
}

/// Where an action runs.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Context {
    pub pane: Option<crate::PaneId>,
    /// Who asked: a socket client's name (`Thurm.app`, `thurm-cli`), or `plugin:<name>`.
    pub origin: String,
    /// How many actions deep this one runs (an action may run others).
    pub depth: u32,
    /// The `[[remote]]` host whose pane the person is in: `pane` is then none, since this
    /// daemon's ids mean nothing there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The text selected in the pane, when an app runs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    /// The file the mouse pointer is over in the pane, as written there (relative to the
    /// pane's directory, unless it is absolute), when an app runs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// The answer of a socket client to something the daemon asked it ([`notice::ACTION_INVOKE`],
/// [`notice::UI_REQUEST`]).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Reply {
    pub id: u64,
    pub result: Value,
    pub error: Option<String>,
}

/// Parameters of [`method::PANES_OPEN`]: a new pane and where to show it. With none of `split`,
/// `float` and `window` it is a new tab.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct OpenPane {
    /// Program and arguments; the shell when empty.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// Start this agent preset instead of `command`.
    pub agent: Option<String>,
    /// Split `target` towards `right`, `down`, `left` or `up`.
    pub split: Option<String>,
    /// Float above `target`'s tab.
    pub float: bool,
    /// Float it at `[x, y, width, height]`, fractions of the tab's area from its top left
    /// corner (as [`crate::FloatPlace::Fraction`]). Implies `float`.
    pub frame: Option<[f64; 4]>,
    /// Float it with a terminal of this many cells, placed from `target`'s first cell (as
    /// [`crate::FloatPlace::Cells`] anchored at `target`). Implies `float`.
    pub cells: Option<FloatCells>,
    /// In a new workspace.
    pub window: bool,
    /// The pane to split, float above, or take the working directory from (the focused one).
    pub target: Option<crate::PaneId>,
    /// The new pane's share of a split in percent.
    pub percent: Option<u8>,
    /// Keep the focus where it is.
    pub background: bool,
    /// Keep the pane open after its program exits.
    pub hold: bool,
    /// Do not show it at all: the caller places it later.
    pub detached: bool,
}

/// [`OpenPane::cells`]: a floating pane's terminal `cols` (at least 2) by `rows` cells, its
/// first cell `col` cells right of and `row` cells below the target's first cell. Either may be
/// negative or past the target's edge: the app keeps the card inside the tab.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(default)]
pub struct FloatCells {
    pub col: i32,
    pub row: i32,
    pub cols: u16,
    pub rows: u16,
}

/// What an app shows for a plugin, sent with [`notice::UI_REQUEST`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiRequest {
    Toast {
        id: u64,
        text: String,
        pane: Option<crate::PaneId>,
    },
    Pick {
        id: u64,
        #[serde(flatten)]
        pick: Pick,
    },
    Input {
        id: u64,
        #[serde(flatten)]
        input: Input,
    },
    Confirm {
        id: u64,
        #[serde(flatten)]
        confirm: Confirm,
    },
    /// Open a forward ([`method::FORWARDS_OPEN`]); answered with its [`ForwardInfo`] and `new`.
    /// Only to an app that said [`crate::CAP_EXT_FORWARDS`], as are the next.
    ForwardOpen {
        id: u64,
        #[serde(flatten)]
        open: ForwardOpen,
    },
    /// Close forward `forward` ([`method::FORWARDS_CLOSE`]); answered with its [`ForwardInfo`].
    ForwardClose { id: u64, forward: u64 },
}

/// A remote host's TCP port opened on this Mac's loopback by the app.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ForwardInfo {
    /// The app's id for it, for [`method::FORWARDS_CLOSE`].
    pub id: u64,
    /// The `[[remote]]` host.
    pub host: String,
    pub remote_port: u16,
    /// The port on `127.0.0.1`.
    pub local_port: u16,
    /// Where it opens: the link it was last opened for, on its local port
    /// (`http://localhost:<local_port>/app`), else `http://localhost:<local_port>/`.
    pub url: String,
}

/// Parameters of [`method::FORWARDS_OPEN`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ForwardOpen {
    /// The `[[remote]]` host.
    pub host: String,
    pub port: u16,
    /// Open its URL in the browser too.
    pub browse: bool,
}

/// Parameters of [`method::FORWARDS_REPORT`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ForwardsReport {
    /// Every forward open now.
    pub forwards: Vec<ForwardInfo>,
    /// What changed, when one did: `{change: "opened"|"closed", forward, why?}`. Its `why`
    /// goes with the `forward_closed` of that forward.
    pub change: Value,
}

/// Parameters of [`method::UI_PICK`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Pick {
    pub title: String,
    pub placeholder: String,
    pub items: Vec<PickItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PickItem {
    /// Returned for the picked item; its place in the list is returned too.
    pub id: Value,
    pub title: String,
    pub detail: String,
}

/// Parameters of [`method::UI_INPUT`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Input {
    pub title: String,
    pub placeholder: String,
    /// What the field starts with.
    pub value: String,
}

/// Parameters of [`method::UI_CONFIRM`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    /// The buttons' titles; "OK" and "Cancel" when empty.
    pub ok: String,
    pub cancel: String,
}

/// Parameters of [`method::EXEC`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Exec {
    /// Program and arguments, run through the login shell like a pane's command.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// Written to its standard input.
    pub stdin: Option<String>,
    /// Stopped (SIGTERM, then SIGKILL) when it runs longer.
    pub timeout_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ExecResult {
    /// Exited with 0.
    pub ok: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// A plugin as [`method::PLUGINS_LIST`] reports it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    /// `lua` or `process`.
    pub kind: String,
    /// Its directory, or its file for a plugin that is only one.
    pub path: String,
    /// `running`, `disabled`, `failed` or `stopped`; `broken` for an entry of the plugins
    /// directory that is not a plugin that could run (`error` says why).
    pub state: String,
    /// Why it failed (or its last error while running).
    pub error: Option<String>,
    /// The actions it provides.
    pub actions: Vec<String>,
}

/// Parameters of [`method::SCRIPT_RUN`].
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct RunScript {
    /// The script's text.
    pub source: String,
    /// What to call it in messages (its file name).
    pub name: String,
    /// `thurm.args` in the script.
    pub args: Vec<String>,
    /// The directory `require` looks in.
    pub dir: Option<String>,
    /// Modules for `require` brought along, by path relative to the script (`lib/util.lua`):
    /// a script run on another host has no directory there.
    pub modules: std::collections::BTreeMap<String, String>,
    /// `thurm.pane` in the script: the pane it was started from.
    pub pane: Option<crate::PaneId>,
}

/// Whether `method` can wait (on a program, another client, the user): a daemon answers it off
/// the connection's reader thread, and a script waits for it without stopping others.
pub fn method_blocks(method: &str) -> bool {
    matches!(
        method,
        method::REQUEST
            | method::PANES_OPEN
            | method::ACTIONS_RUN
            | method::UI_PICK
            | method::UI_INPUT
            | method::UI_CONFIRM
            | method::EXEC
            | method::SCRIPT_RUN
            | method::PLUGINS_RELOAD
            | method::PLUGINS_ENABLE
            | method::PLUGINS_DISABLE
            | method::PORTS_LIST
            | method::FORWARDS_OPEN
            | method::FORWARDS_CLOSE
    )
}

/// Whether `name` can name an action: 1 to 64 letters, digits, `.`, `_`, `-`, `:` and spaces,
/// neither starting nor ending with a space.
pub fn valid_action_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name.trim() == name
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | ' '))
}

/// Whether `name` can name a plugin (it names files and appears in commands): 1 to 48 ASCII
/// letters, digits, `-` and `_`, starting with a letter or digit.
pub fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_unknown_fields_are_tolerated() {
        let a: ActionInfo = serde_json::from_str(r#"{"name":"x","later":1}"#).unwrap();
        assert_eq!(a.name, "x");
        assert_eq!(a.title(), "x");
        assert!(a.args.is_null());
        assert!(!serde_json::to_string(&a).unwrap().contains("args"));
    }

    #[test]
    fn ui_requests_are_tagged_by_kind() {
        let r = UiRequest::Pick {
            id: 3,
            pick: Pick {
                title: "Branch".into(),
                items: vec![PickItem {
                    title: "main".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "pick");
        assert_eq!(v["id"], 3);
        assert_eq!(v["items"][0]["title"], "main");
        assert_eq!(serde_json::from_value::<UiRequest>(v).unwrap(), r);
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_action_name("git.sync"));
        assert!(valid_action_name("Open lazygit"));
        assert!(!valid_action_name(" x"));
        assert!(!valid_action_name(""));
        assert!(!valid_action_name("a/b"));
        assert!(valid_plugin_name("git-worktrees"));
        assert!(!valid_plugin_name("../x"));
        assert!(!valid_plugin_name("-x"));
    }
}
