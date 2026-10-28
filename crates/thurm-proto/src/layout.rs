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
    /// A pane of `root` or a floating one (ids are unique per host, and a tab's panes share
    /// one host).
    pub focused: PaneId,
    /// Pane of `root` shown zoomed (maximized within the tab).
    #[serde(default)]
    pub zoomed: Option<PaneId>,
    /// A handoff tab: a remote agent working on a local repository (see `thurm handoff`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
    /// The agent wall: the app gathered the agents of these tabs here, and puts them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall: Option<Vec<WallOrigin>>,
    /// Panes floating above `root`, back to front.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub floating: Vec<FloatingPane>,
    /// The floating panes are hidden (their programs keep running) until shown again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub floating_hidden: bool,
}

/// A pane lifted out of a tab's splits, shown as a card above them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FloatingPane {
    pub id: PaneId,
    /// The `[[remote]]` host whose daemon runs the pane; `None` for this daemon's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `[x, y, width, height]` as fractions of the tab's area, from its top left corner.
    pub frame: [f64; 4],
    /// Shown over the whole tab.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub zoomed: bool,
}

impl TabLayout {
    /// This daemon's panes: the leaves of `root`, then the floating ones.
    pub fn panes(&self, out: &mut Vec<PaneId>) {
        self.root.panes(out);
        out.extend(
            self.floating
                .iter()
                .filter(|f| f.host.is_none())
                .map(|f| f.id),
        );
    }
}

/// A tab the agent wall took agents from: its place among the window's tabs and how it looked.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WallOrigin {
    pub index: usize,
    /// A pane of the tab before it (`None`: it was the first), which places it among tabs
    /// opened since; `index` when that pane is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<PaneId>,
    pub tab: TabLayout,
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
            t.panes(&mut out);
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
            t.floating.retain(|f| f.host.is_some() || keep(f.id));
            let root = match t.root.remap(&|id| keep(id).then_some(id)) {
                Some(root) => root,
                // The splits are gone: the frontmost floating pane takes their place.
                None => {
                    let f = t.floating.pop()?;
                    LayoutNode::Pane {
                        id: f.id,
                        host: f.host,
                    }
                }
            };
            let mut leaves = Vec::new();
            root.leaves(&mut leaves);
            let tiled: Vec<PaneId> = leaves.iter().map(|(_, id)| *id).collect();
            if !tiled.contains(&t.focused) && !t.floating.iter().any(|f| f.id == t.focused) {
                t.focused = tiled[0];
            }
            if t.zoomed.is_some_and(|z| !tiled.contains(&z)) {
                t.zoomed = None;
            }
            t.root = root;
            t.floating_hidden &= !t.floating.is_empty();
            t.wall = t.wall.take().map(|origins| {
                origins
                    .into_iter()
                    .filter_map(|o| {
                        let tab = retain_tabs(vec![o.tab], keep).pop()?;
                        Some(WallOrigin { tab, ..o })
                    })
                    .collect()
            });
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
                        wall: None,
                        floating: Vec::new(),
                        floating_hidden: false,
                    },
                    TabLayout {
                        title: Some("logs".into()),
                        root: LayoutNode::local(3),
                        focused: 3,
                        zoomed: None,
                        handoff: None,
                        wall: None,
                        floating: Vec::new(),
                        floating_hidden: false,
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
                        wall: None,
                        floating: Vec::new(),
                        floating_hidden: false,
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
                wall: None,
                floating: Vec::new(),
                floating_hidden: false,
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
            wall: None,
            floating: Vec::new(),
            floating_hidden: false,
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

    #[test]
    fn wall_keeps_its_origins() {
        let tab = |root: LayoutNode, focused| TabLayout {
            title: None,
            root,
            focused,
            zoomed: None,
            handoff: None,
            wall: None,
            floating: Vec::new(),
            floating_hidden: false,
        };
        let split = |a, b| LayoutNode::Split {
            dir: SplitDir::Down,
            ratio: 0.3,
            first: Box::new(LayoutNode::local(a)),
            second: Box::new(LayoutNode::local(b)),
        };
        let mut l = sample();
        // Agents 6 and 8 left a tab of their own and a tab where shell 7 stayed.
        let mut wall = tab(split(6, 8), 6);
        wall.title = Some("Agents".into());
        wall.wall = Some(vec![
            WallOrigin {
                index: 2,
                after: Some(3),
                tab: tab(LayoutNode::local(6), 6),
            },
            WallOrigin {
                index: 3,
                after: Some(6),
                tab: tab(split(7, 8), 8),
            },
        ]);
        l.windows[0].tabs.push(wall);
        l.windows[0].tabs.push(tab(LayoutNode::local(7), 7));
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
        // A tab without a wall says nothing of one, and older layouts have none.
        assert_eq!(json.matches(r#""wall""#).count(), 1);
        // Agent 6 ended: its tab has nothing to come back to; 8 still goes back next to 7.
        l.retain_panes(&|id| id != 6);
        let origins = l.windows[0].tabs[2].wall.as_ref().unwrap();
        assert_eq!(origins.len(), 1);
        assert_eq!((origins[0].index, origins[0].after), (3, Some(6)));
        assert_eq!(origins[0].tab.root, split(7, 8));
    }

    #[test]
    fn floating_panes_belong_to_their_tab() {
        let card = |id, host: Option<&str>| FloatingPane {
            id,
            host: host.map(Into::into),
            frame: [0.2, 0.2, 0.5, 0.5],
            zoomed: false,
        };
        let mut l = sample();
        // A tab without floating panes says nothing of them, and older layouts have none.
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("floating"));
        let tab = &mut l.windows[0].tabs[1];
        tab.floating = vec![card(6, None), card(9, Some("devbox"))];
        tab.focused = 6;
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains(r#""floating":[{"id":6,"frame":[0.2,0.2,0.5,0.5]}"#));
        assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
        // Only local floating panes count, after the tab's splits.
        assert_eq!(l.panes(), vec![1, 2, 3, 6, 4, 5]);
        // A focused floating pane stays focused while it lives.
        l.retain_panes(&|id| id != 1);
        assert_eq!(l.windows[0].tabs[1].focused, 6);
        l.retain_panes(&|id| id != 6);
        let tab = &l.windows[0].tabs[1];
        assert_eq!(tab.focused, 3);
        assert_eq!(tab.floating, vec![card(9, Some("devbox"))]);
    }

    #[test]
    fn hidden_floating_panes_stay_hidden_while_there_are_any() {
        let card = |id| FloatingPane {
            id,
            host: None,
            frame: [0.2, 0.2, 0.5, 0.5],
            zoomed: false,
        };
        let mut l = sample();
        let tab = &mut l.windows[0].tabs[1];
        tab.floating = vec![card(6), card(7)];
        tab.floating_hidden = true;
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains(r#""floating_hidden":true"#));
        assert_eq!(serde_json::from_str::<Layout>(&json).unwrap(), l);
        l.retain_panes(&|id| id != 6);
        assert!(l.windows[0].tabs[1].floating_hidden);
        // The last one gone: a new floating pane shows.
        l.retain_panes(&|id| id != 7);
        assert!(!l.windows[0].tabs[1].floating_hidden);
        assert!(!serde_json::to_string(&l).unwrap().contains("floating"));
    }

    #[test]
    fn a_floating_pane_takes_over_the_splits() {
        let card = |id| FloatingPane {
            id,
            host: None,
            frame: [0.1, 0.1, 0.4, 0.4],
            zoomed: true,
        };
        let mut l = sample();
        let tab = &mut l.windows[0].tabs[1];
        tab.floating = vec![card(6), card(7)];
        tab.zoomed = Some(3);
        // Pane 3 ended: the frontmost floating pane fills the tab, and nothing is zoomed.
        l.retain_panes(&|id| id != 3);
        let tab = &l.windows[0].tabs[1];
        assert_eq!(tab.root, LayoutNode::local(7));
        assert_eq!(tab.floating, vec![card(6)]);
        assert_eq!((tab.focused, tab.zoomed), (7, None));
        // The tab goes with its last pane.
        l.retain_panes(&|id| id != 6 && id != 7);
        assert_eq!(l.windows[0].tabs.len(), 1);
    }
}
