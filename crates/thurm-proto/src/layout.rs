//! Window / tab / split layout. Owned by the GUI, persisted by the daemon.
//!
//! The local daemon stores the app's whole layout, remote workspaces included: a leaf with a
//! `host` is a pane of that `[[remote]]` host's daemon, which this daemon knows nothing about
//! and keeps as is.
//!
//! Always serialized as JSON (it crosses the FFI boundary to Swift as JSON and is stored
//! on disk as JSON), so internally tagged enums are fine here.

use serde::{Deserialize, Serialize};

use crate::PaneId;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Layout {
    #[serde(default)]
    pub windows: Vec<WindowLayout>,
    /// Every workspace (a named set of tabs; a window shows one at a time). Tabs of a
    /// workspace shown in a window are in that window; the rest keep theirs here, with their
    /// panes still running.
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// The quick terminal's tab (shown on a global hotkey, outside any window or workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick: Option<TabLayout>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Workspace {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    /// Unix seconds this workspace was last shown.
    #[serde(default)]
    pub last_active: u64,
    /// Its tabs while no window shows it.
    #[serde(default)]
    pub tabs: Vec<TabLayout>,
    #[serde(default)]
    pub selected_tab: usize,
    /// The `[[remote]]` host whose panes it shows (a remote workspace); `None` for this
    /// machine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct WindowLayout {
    /// `[x, y, width, height]` in screen points.
    #[serde(default)]
    pub frame: Option<[f64; 4]>,
    #[serde(default)]
    pub tabs: Vec<TabLayout>,
    #[serde(default)]
    pub selected_tab: usize,
    #[serde(default)]
    pub fullscreen: bool,
    /// The workspace this window shows (0 = none yet; one is assigned on restore).
    #[serde(default)]
    pub workspace: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TabLayout {
    /// User supplied tab title; `None` follows the focused pane's title.
    #[serde(default)]
    pub title: Option<String>,
    pub root: LayoutNode,
    /// A pane of `root` (ids are unique per host, and a tab's panes share one host).
    pub focused: PaneId,
    /// Pane shown zoomed (maximized within the tab).
    #[serde(default)]
    pub zoomed: Option<PaneId>,
    /// A handoff tab: a remote agent working on a local repository (see `thurm handoff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNode {
    Pane {
        id: PaneId,
        /// The `[[remote]]` host whose daemon runs the pane; `None` for this daemon's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    Split {
        dir: SplitDir,
        /// Fraction of the space given to `first` (0..1).
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// `Right` puts the new pane to the right (a vertical divider), `Down` below.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Right,
    Down,
    Left,
    Up,
}

impl LayoutNode {
    /// A leaf for one of this daemon's panes.
    pub fn local(id: PaneId) -> Self {
        LayoutNode::Pane { id, host: None }
    }

    /// This daemon's panes (leaves without a host).
    pub fn panes(&self, out: &mut Vec<PaneId>) {
        match self {
            LayoutNode::Pane { id, host: None } => out.push(*id),
            LayoutNode::Pane { .. } => {}
            LayoutNode::Split { first, second, .. } => {
                first.panes(out);
                second.panes(out);
            }
        }
    }

    /// Every leaf as (host, id).
    pub fn leaves<'a>(&'a self, out: &mut Vec<(Option<&'a str>, PaneId)>) {
        match self {
            LayoutNode::Pane { id, host } => out.push((host.as_deref(), *id)),
            LayoutNode::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    /// Replace this daemon's pane ids according to `map`, dropping panes that map to `None`;
    /// remote leaves stay. Returns `None` if the whole subtree disappears.
    pub fn remap(self, map: &dyn Fn(PaneId) -> Option<PaneId>) -> Option<LayoutNode> {
        match self {
            LayoutNode::Pane { id, host: None } => map(id).map(LayoutNode::local),
            remote @ LayoutNode::Pane { .. } => Some(remote),
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => match (first.remap(map), second.remap(map)) {
                (Some(a), Some(b)) => Some(LayoutNode::Split {
                    dir,
                    ratio,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
        }
    }
}

impl Layout {
    /// This daemon's panes (remote leaves are left out).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        let tabs = self.windows.iter().flat_map(|w| &w.tabs);
        let tabs = tabs.chain(self.workspaces.iter().flat_map(|w| &w.tabs));
        for t in tabs.chain(&self.quick) {
            t.root.panes(&mut out);
        }
        out
    }

    /// Drop this daemon's panes for which `keep` returns false (remote leaves stay), removing
    /// empty tabs, windows and workspaces (a workspace stays while it has tabs or a window
    /// shows it).
    pub fn retain_panes(&mut self, keep: &dyn Fn(PaneId) -> bool) {
        for w in &mut self.windows {
            w.tabs = retain_tabs(std::mem::take(&mut w.tabs), keep);
            if w.selected_tab >= w.tabs.len() {
                w.selected_tab = w.tabs.len().saturating_sub(1);
            }
        }
        self.windows.retain(|w| !w.tabs.is_empty());
        for ws in &mut self.workspaces {
            ws.tabs = retain_tabs(std::mem::take(&mut ws.tabs), keep);
            if ws.selected_tab >= ws.tabs.len() {
                ws.selected_tab = ws.tabs.len().saturating_sub(1);
            }
        }
        let shown: Vec<u64> = self.windows.iter().map(|w| w.workspace).collect();
        self.workspaces
            .retain(|ws| !ws.tabs.is_empty() || shown.contains(&ws.id));
        self.quick = retain_tabs(self.quick.take().into_iter().collect(), keep).pop();
    }
}

fn retain_tabs(tabs: Vec<TabLayout>, keep: &dyn Fn(PaneId) -> bool) -> Vec<TabLayout> {
    tabs.into_iter()
        .filter_map(|mut t| {
            let root = t.root.remap(&|id| keep(id).then_some(id))?;
            let mut leaves = Vec::new();
            root.leaves(&mut leaves);
            let ids: Vec<PaneId> = leaves.iter().map(|(_, id)| *id).collect();
            if !ids.contains(&t.focused) {
                t.focused = ids[0];
            }
            if t.zoomed.is_some_and(|z| !ids.contains(&z)) {
                t.zoomed = None;
            }
            t.root = root;
            Some(t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Layout {
        Layout {
            windows: vec![WindowLayout {
                frame: Some([0.0, 0.0, 800.0, 600.0]),
                tabs: vec![
                    TabLayout {
                        title: None,
                        root: LayoutNode::Split {
                            dir: SplitDir::Right,
                            ratio: 0.5,
                            first: Box::new(LayoutNode::local(1)),
                            second: Box::new(LayoutNode::local(2)),
                        },
                        focused: 2,
                        zoomed: None,
                        handoff: None,
                    },
                    TabLayout {
                        title: Some("logs".into()),
                        root: LayoutNode::local(3),
                        focused: 3,
                        zoomed: None,
                        handoff: None,
                    },
                ],
                selected_tab: 1,
                fullscreen: false,
                workspace: 1,
            }],
            workspaces: vec![
                Workspace {
                    id: 1,
                    name: "shown".into(),
                    ..Default::default()
                },
                Workspace {
                    id: 2,
                    name: "hidden".into(),
                    last_active: 10,
                    tabs: vec![TabLayout {
                        title: None,
                        root: LayoutNode::local(4),
                        focused: 4,
                        zoomed: None,
                        handoff: None,
                    }],
                    selected_tab: 0,
                    host: None,
                },
            ],
            quick: Some(TabLayout {
                title: None,
                root: LayoutNode::local(5),
                focused: 5,
                zoomed: None,
                handoff: None,
            }),
        }
    }

    #[test]
    fn hidden_workspaces_hold_panes() {
        let mut l = sample();
        assert_eq!(l.panes(), vec![1, 2, 3, 4, 5]);
        // The hidden workspace goes with its last pane; the shown one stays with its window.
        l.retain_panes(&|id| id != 4);
        assert_eq!(l.workspaces.len(), 1);
        assert_eq!(l.workspaces[0].id, 1);
        l.retain_panes(&|_| false);
        assert!(l.windows.is_empty() && l.workspaces.is_empty());
        // Older layouts without workspaces or a quick terminal still parse.
        let old: Layout = serde_json::from_str(r#"{"windows":[]}"#).unwrap();
        assert!(old.workspaces.is_empty() && old.quick.is_none());
    }

    #[test]
    fn json_shape() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(json.contains(r#""type":"split""#));
        assert!(json.contains(r#""dir":"right""#));
        let back: Layout = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn retain_collapses_splits() {
        let mut l = sample();
        l.retain_panes(&|id| id != 2);
        assert_eq!(l.windows[0].tabs[0].root, LayoutNode::local(1));
        assert_eq!(l.windows[0].tabs[0].focused, 1);
        l.retain_panes(&|id| id == 1);
        assert_eq!(l.windows[0].tabs.len(), 1);
        assert_eq!(l.windows[0].selected_tab, 0);
        l.retain_panes(&|_| false);
        assert!(l.windows.is_empty());
    }

    #[test]
    fn retain_drops_ended_quick_terminal() {
        let mut l = sample();
        l.retain_panes(&|id| id != 1);
        assert_eq!(l.quick.as_ref().unwrap().focused, 5);
        l.retain_panes(&|id| id != 5);
        assert!(l.quick.is_none());
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("quick"));
    }

    #[test]
    fn remote_leaves_survive_local_pruning() {
        let remote = |id| LayoutNode::Pane {
            id,
            host: Some("devbox".into()),
        };
        let mut l = sample();
        // Remote pane 1 collides with local pane 1; both live in their own tabs.
        l.windows[0].tabs.push(TabLayout {
            title: None,
            root: LayoutNode::Split {
                dir: SplitDir::Down,
                ratio: 0.5,
                first: Box::new(remote(1)),
                second: Box::new(remote(7)),
            },
            focused: 7,
            zoomed: None,
            handoff: Some("agent/quiet-otter".into()),
        });
        l.workspaces[0].host = Some("devbox".into());
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains(r#"{"type":"pane","id":1,"host":"devbox"}"#));
        assert!(
            json.contains(r#"{"type":"pane","id":1}"#),
            "local leaves have no host"
        );
        assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
        // Only local panes count, and only they are pruned.
        assert_eq!(l.panes(), vec![1, 2, 3, 4, 5]);
        l.retain_panes(&|_| false);
        assert_eq!(l.windows.len(), 1);
        let tab = &l.windows[0].tabs[0];
        let mut leaves = Vec::new();
        tab.root.leaves(&mut leaves);
        assert_eq!(leaves, vec![(Some("devbox"), 1), (Some("devbox"), 7)]);
        assert_eq!(tab.focused, 7);
        assert_eq!(tab.handoff.as_deref(), Some("agent/quiet-otter"));
        assert_eq!(l.workspaces[0].host.as_deref(), Some("devbox"));
    }
}
