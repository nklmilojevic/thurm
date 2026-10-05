//! Toolkit-free logic shared by the window code: the split tree (as SplitView.swift keeps it),
//! workspaces, and the text the UI derives from pane info (titles, paths, agent details).

use std::time::{SystemTime, UNIX_EPOCH};

use thurm_proto::layout::{LayoutNode, SplitDir, TabLayout};
use thurm_proto::{AgentStatus, PaneInfo};

use crate::core::{HostId, LOCAL, PaneKey};

// MARK: - Geometry

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn max_x(&self) -> f64 {
        self.x + self.w
    }
    pub fn max_y(&self) -> f64 {
        self.y + self.h
    }
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

// MARK: - Split tree

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Side by side (a vertical divider).
    Horizontal,
    /// Stacked (a horizontal divider).
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn parse(s: &str) -> Direction {
        match s {
            "left" => Direction::Left,
            "up" => Direction::Up,
            "down" => Direction::Down,
            _ => Direction::Right,
        }
    }
}

pub const MIN_RATIO: f64 = 0.05;
pub const MAX_RATIO: f64 = 0.95;
/// Divider thickness between split panes (logical px).
pub const GAP: f64 = 1.0;

/// A tab's panes; children are in screen order (left/top first), `ratio` is the first child's
/// share.
#[derive(Clone, Debug, PartialEq)]
pub enum SplitNode {
    Leaf(PaneKey),
    Split {
        axis: Axis,
        ratio: f64,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

/// A divider: the line between two children, and the tree path of its split.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    pub line: Rect,
    pub axis: Axis,
    pub path: Vec<bool>,
    /// The split's whole area (to turn a drag position into a ratio).
    pub container: Rect,
}

impl SplitNode {
    /// From the stored layout; `host` tags leaves without one (`None` = this machine's).
    pub fn from_layout(node: &LayoutNode, default_host: &str) -> SplitNode {
        match node {
            LayoutNode::Pane { id, host } => {
                SplitNode::Leaf(PaneKey::new(host.as_deref().unwrap_or(default_host), *id))
            }
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let a = SplitNode::from_layout(first, default_host);
                let b = SplitNode::from_layout(second, default_host);
                let r = if ratio.is_finite() { *ratio } else { 0.5 };
                let (axis, first, second, ratio) = match dir {
                    SplitDir::Right => (Axis::Horizontal, a, b, r),
                    SplitDir::Down => (Axis::Vertical, a, b, r),
                    SplitDir::Left => (Axis::Horizontal, b, a, 1.0 - r),
                    SplitDir::Up => (Axis::Vertical, b, a, 1.0 - r),
                };
                SplitNode::Split {
                    axis,
                    ratio: ratio.clamp(MIN_RATIO, MAX_RATIO),
                    first: Box::new(first),
                    second: Box::new(second),
                }
            }
        }
    }

    /// For the stored layout: `right`/`down` with screen-order children; local leaves have no
    /// host.
    pub fn to_layout(&self) -> LayoutNode {
        match self {
            SplitNode::Leaf(k) => LayoutNode::Pane {
                id: k.id,
                host: k.is_remote().then(|| k.host.clone()),
            },
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutNode::Split {
                dir: match axis {
                    Axis::Horizontal => SplitDir::Right,
                    Axis::Vertical => SplitDir::Down,
                },
                ratio: *ratio,
                first: Box::new(first.to_layout()),
                second: Box::new(second.to_layout()),
            },
        }
    }

    pub fn panes(&self) -> Vec<PaneKey> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneKey>) {
        match self {
            SplitNode::Leaf(k) => out.push(k.clone()),
            SplitNode::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn contains(&self, key: &PaneKey) -> bool {
        match self {
            SplitNode::Leaf(k) => k == key,
            SplitNode::Split { first, second, .. } => first.contains(key) || second.contains(key),
        }
    }

    /// Replaces leaf `target` with a split of it and `pane` (new pane after it for right/down,
    /// before it for left/up).
    pub fn split(self, target: &PaneKey, pane: PaneKey, dir: Direction) -> SplitNode {
        match self {
            SplitNode::Leaf(k) if &k == target => {
                let axis = match dir {
                    Direction::Left | Direction::Right => Axis::Horizontal,
                    Direction::Up | Direction::Down => Axis::Vertical,
                };
                let (first, second) = match dir {
                    Direction::Right | Direction::Down => (SplitNode::Leaf(k), SplitNode::Leaf(pane)),
                    Direction::Left | Direction::Up => (SplitNode::Leaf(pane), SplitNode::Leaf(k)),
                };
                SplitNode::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                }
            }
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => SplitNode::Split {
                axis,
                ratio,
                first: Box::new(first.split(target, pane.clone(), dir)),
                second: Box::new(second.split(target, pane, dir)),
            },
            leaf => leaf,
        }
    }

    /// The tree without `key`; `None` when nothing is left.
    pub fn remove(self, key: &PaneKey) -> Option<SplitNode> {
        match self {
            SplitNode::Leaf(k) if &k == key => None,
            leaf @ SplitNode::Leaf(_) => Some(leaf),
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(key), second.remove(key)) {
                (Some(a), Some(b)) => Some(SplitNode::Split {
                    axis,
                    ratio,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (a, b) => a.or(b),
            },
        }
    }

    /// Frames of every pane and the dividers inside `rect`.
    pub fn layout(&self, rect: Rect, out: &mut Vec<(PaneKey, Rect)>, dividers: &mut Vec<Divider>) {
        self.layout_at(rect, &mut Vec::new(), out, dividers);
    }

    fn layout_at(
        &self,
        rect: Rect,
        path: &mut Vec<bool>,
        out: &mut Vec<(PaneKey, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            SplitNode::Leaf(k) => out.push((k.clone(), rect)),
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, line, b) = divide(rect, *axis, *ratio);
                dividers.push(Divider {
                    line,
                    axis: *axis,
                    path: path.clone(),
                    container: rect,
                });
                path.push(false);
                first.layout_at(a, path, out, dividers);
                path.pop();
                path.push(true);
                second.layout_at(b, path, out, dividers);
                path.pop();
            }
        }
    }

    /// Sets the ratio of the split at `path`.
    pub fn set_ratio(&mut self, path: &[bool], value: f64) {
        match self {
            SplitNode::Leaf(_) => {}
            SplitNode::Split {
                ratio,
                first,
                second,
                ..
            } => match path.split_first() {
                None => *ratio = value.clamp(MIN_RATIO, MAX_RATIO),
                Some((false, rest)) => first.set_ratio(rest, value),
                Some((true, rest)) => second.set_ratio(rest, value),
            },
        }
    }

    /// Moves the divider of the nearest ancestor split of `key` on `axis` by `delta`.
    /// Returns whether one was found.
    pub fn resize(&mut self, key: &PaneKey, axis: Axis, delta: f64) -> bool {
        match self {
            SplitNode::Leaf(_) => false,
            SplitNode::Split {
                axis: a,
                ratio,
                first,
                second,
            } => {
                let child = if first.contains(key) {
                    first
                } else if second.contains(key) {
                    second
                } else {
                    return false;
                };
                if child.resize(key, axis, delta) {
                    return true;
                }
                if *a == axis {
                    *ratio = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
                    return true;
                }
                false
            }
        }
    }

    /// Every split gets the ratio that gives each pane along its axis the same space.
    pub fn equalize(&mut self) {
        if let SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            first.equalize();
            second.equalize();
            let w1 = first.weight(*axis);
            let w2 = second.weight(*axis);
            *ratio = (w1 / (w1 + w2)).clamp(MIN_RATIO, MAX_RATIO);
        }
    }

    fn weight(&self, along: Axis) -> f64 {
        match self {
            SplitNode::Leaf(_) => 1.0,
            SplitNode::Split {
                axis,
                first,
                second,
                ..
            } => {
                if *axis == along {
                    first.weight(along) + second.weight(along)
                } else {
                    first.weight(along).max(second.weight(along))
                }
            }
        }
    }
}

/// Splits `rect` at `ratio` with a `GAP`-wide line between the parts.
pub fn divide(rect: Rect, axis: Axis, ratio: f64) -> (Rect, Rect, Rect) {
    let r = ratio.clamp(MIN_RATIO, MAX_RATIO);
    match axis {
        Axis::Horizontal => {
            let avail = (rect.w - GAP).max(0.0);
            let w1 = (avail * r).round();
            (
                Rect::new(rect.x, rect.y, w1, rect.h),
                Rect::new(rect.x + w1, rect.y, GAP, rect.h),
                Rect::new(rect.x + w1 + GAP, rect.y, avail - w1, rect.h),
            )
        }
        Axis::Vertical => {
            let avail = (rect.h - GAP).max(0.0);
            let h1 = (avail * r).round();
            (
                Rect::new(rect.x, rect.y, rect.w, h1),
                Rect::new(rect.x, rect.y + h1, rect.w, GAP),
                Rect::new(rect.x, rect.y + h1 + GAP, rect.w, avail - h1),
            )
        }
    }
}

/// The pane `dir` of `current` (SplitView.swift `moveFocus`): overlapping neighbours first,
/// then the nearest edge, then the nearest center.
pub fn neighbor(frames: &[(PaneKey, Rect)], current: &PaneKey, dir: Direction) -> Option<PaneKey> {
    let cur = frames.iter().find(|(k, _)| k == current)?.1;
    let (ccx, ccy) = cur.center();
    let mut best: Option<(f64, &PaneKey)> = None;
    for (k, f) in frames {
        if k == current {
            continue;
        }
        let (overlap, distance) = match dir {
            Direction::Left => {
                if f.max_x() > cur.x + 2.0 {
                    continue;
                }
                (f.max_y() > cur.y && f.y < cur.max_y(), cur.x - f.max_x())
            }
            Direction::Right => {
                if f.x < cur.max_x() - 2.0 {
                    continue;
                }
                (f.max_y() > cur.y && f.y < cur.max_y(), f.x - cur.max_x())
            }
            Direction::Up => {
                if f.max_y() > cur.y + 2.0 {
                    continue;
                }
                (f.max_x() > cur.x && f.x < cur.max_x(), cur.y - f.max_y())
            }
            Direction::Down => {
                if f.y < cur.max_y() - 2.0 {
                    continue;
                }
                (f.max_x() > cur.x && f.x < cur.max_x(), f.y - cur.max_y())
            }
        };
        let (fx, fy) = f.center();
        let score = if overlap { 0.0 } else { 100_000.0 }
            + distance * 10.0
            + ((fx - ccx).powi(2) + (fy - ccy).powi(2)).sqrt();
        if best.as_ref().is_none_or(|(s, _)| score < *s) {
            best = Some((score, k));
        }
    }
    best.map(|(_, k)| k.clone())
}

/// The pane whose center is nearest to `point` (focus after the focused pane closed).
pub fn nearest(frames: &[(PaneKey, Rect)], point: (f64, f64)) -> Option<PaneKey> {
    frames
        .iter()
        .min_by(|(_, a), (_, b)| {
            let da = (a.center().0 - point.0).powi(2) + (a.center().1 - point.1).powi(2);
            let db = (b.center().0 - point.0).powi(2) + (b.center().1 - point.1).powi(2);
            da.total_cmp(&db)
        })
        .map(|(k, _)| k.clone())
}

// MARK: - Workspaces

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
    /// Unix seconds.
    pub last_active: u64,
    /// Its tabs while the window doesn't show it.
    pub hidden_tabs: Vec<TabLayout>,
    pub hidden_selected: usize,
    pub host: HostId,
}

impl Workspace {
    pub fn is_remote(&self) -> bool {
        self.host != LOCAL
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "cosmic", "crisp", "dapper", "eager", "fuzzy",
    "gentle", "golden", "happy", "hidden", "jolly", "keen", "lucky", "mellow", "misty", "nimble",
    "noble", "polar", "proud", "quiet", "rapid", "rusty", "shiny", "silent", "sly", "sunny",
    "swift", "tidy", "velvet", "vivid", "wild", "witty", "young", "zesty",
];
const NOUNS: &[&str] = &[
    "badger", "beacon", "comet", "cedar", "falcon", "fern", "fox", "harbor", "heron", "island",
    "koala", "lark", "lynx", "maple", "meadow", "moose", "nebula", "orca", "otter", "owl", "panda",
    "pebble", "pine", "puffin", "quartz", "raven", "river", "robin", "sparrow", "summit", "tiger",
    "tundra", "walrus", "willow", "wolf", "yak", "zebra",
];

/// A small xorshift seeded from the clock: names only need to look random.
fn random(seed: &mut u64) -> usize {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed >> 11) as usize
}

/// A free `adjective-noun` name (`workspace-N` when unlucky); a remote host's first workspace
/// takes the host's name.
pub fn workspace_name(taken: &[String], host: &str) -> String {
    let is_taken = |n: &str| taken.iter().any(|t| t == n);
    if host != LOCAL && !is_taken(host) {
        return host.to_string();
    }
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9e37_79b9, |d| d.as_nanos() as u64)
        | 1;
    for _ in 0..64 {
        let codename = format!(
            "{}-{}",
            ADJECTIVES[random(&mut seed) % ADJECTIVES.len()],
            NOUNS[random(&mut seed) % NOUNS.len()]
        );
        let name = if host != LOCAL {
            format!("{host}-{codename}")
        } else {
            codename
        };
        if !is_taken(&name) {
            return name;
        }
    }
    (2..)
        .map(|n| format!("workspace-{n}"))
        .find(|n| !is_taken(n))
        .unwrap_or_default()
}

// MARK: - Pane text

/// The trimmed program title, else the cwd's last component, else "Thurm".
pub fn display_title(info: Option<&PaneInfo>) -> String {
    let Some(info) = info else {
        return "Thurm".into();
    };
    let t = info.title.trim();
    if !t.is_empty() {
        return t.to_string();
    }
    if let Some(cwd) = &info.cwd {
        let last = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        return if last.is_empty() { cwd.clone() } else { last.to_string() };
    }
    "Thurm".into()
}

const SHELLS: &[&str] = &[
    "zsh", "bash", "fish", "sh", "dash", "tcsh", "csh", "ksh", "nu", "pwsh", "elvish", "xonsh",
    "login",
];

/// A program other than the shell runs in the foreground.
pub fn has_running_process(info: &PaneInfo) -> bool {
    if !info.alive {
        return false;
    }
    let Some(fg) = &info.foreground else { return false };
    let name = fg.name.trim_start_matches('-');
    let name = name.rsplit('/').next().unwrap_or(name);
    !name.is_empty() && !SHELLS.contains(&name)
}

pub fn urgency(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Idle => 0,
        AgentStatus::Done => 1,
        AgentStatus::Working => 2,
        AgentStatus::NeedsInput => 3,
    }
}

/// The most urgent agent status among `infos` (first seen on ties).
pub fn tab_status<'a>(infos: impl Iterator<Item = &'a PaneInfo>) -> Option<AgentStatus> {
    let mut best: Option<AgentStatus> = None;
    for s in infos.filter_map(|i| i.agent.as_ref().map(|a| a.status)) {
        if best.is_none_or(|b| urgency(s) > urgency(b)) {
            best = Some(s);
        }
    }
    best
}

pub fn status_prefix(status: Option<AgentStatus>) -> &'static str {
    match status {
        Some(AgentStatus::NeedsInput) => "● ",
        Some(AgentStatus::Working) => "◌ ",
        Some(AgentStatus::Done) => "✓ ",
        _ => "",
    }
}

/// Status dot colors (sidebar, tabs), as the macOS system colors.
pub fn status_color(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Working => "#0a84ff",
        AgentStatus::NeedsInput => "#ff9f0a",
        AgentStatus::Done => "#30d158",
        AgentStatus::Idle => "#8e8e93",
    }
}

pub fn status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Working => "Working",
        AgentStatus::Idle => "Idle",
        AgentStatus::NeedsInput => "NeedsInput",
        AgentStatus::Done => "Done",
    }
}

/// "42s", "3m 05s", "1h 02m".
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// `~` for home, every component but the last cut to one char (two for dotfiles):
/// `~/dev/personal/thurm` → `~/d/p/thurm`.
pub fn abbreviate_path(path: &str, home: Option<&str>) -> String {
    let mut p = path.to_string();
    if let Some(home) = home.filter(|h| !h.is_empty())
        && (p == home || p.starts_with(&format!("{home}/")))
    {
        p = format!("~{}", &p[home.len()..]);
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() <= 2 {
        return p;
    }
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == last || c.is_empty() || *c == "~" {
                c.to_string()
            } else {
                let n = if c.starts_with('.') { 2 } else { 1 };
                c.chars().take(n).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The agent pane's title: its topic, else the program title without leading status glyphs,
/// else the agent's name.
pub fn agent_title(info: &PaneInfo) -> String {
    let Some(agent) = &info.agent else {
        return display_title(Some(info));
    };
    if let Some(t) = agent.topic.as_ref().filter(|t| !t.trim().is_empty()) {
        return t.clone();
    }
    let topic: String = info
        .title
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string();
    if !topic.is_empty() && topic != agent.name {
        return topic;
    }
    agent.name.clone()
}

/// "Needs your permission to use Bash", "Working", "Done in 2m 05s · summary", "Idle".
pub fn status_detail(info: &PaneInfo) -> String {
    let Some(agent) = &info.agent else {
        return String::new();
    };
    match agent.status {
        AgentStatus::NeedsInput => agent
            .message
            .as_deref()
            .map(|m| shorten_message(m, &agent.name))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "Needs input".into()),
        AgentStatus::Working => "Working".into(),
        AgentStatus::Done => {
            let mut s = match agent.turn_ms {
                Some(ms) => format!("Done in {}", format_duration(ms)),
                None => "Done".into(),
            };
            if let Some(m) = agent.message.as_ref().filter(|m| !m.is_empty()) {
                s.push_str(" · ");
                s.push_str(m);
            }
            s
        }
        AgentStatus::Idle => "Idle".into(),
    }
}

/// "Claude needs your permission…" → "Needs your permission…".
fn shorten_message(message: &str, agent_name: &str) -> String {
    let first = agent_name.split_whitespace().next().unwrap_or("");
    let mut m = message.trim();
    if !first.is_empty()
        && let Some(rest) = m.strip_prefix(first)
        && let Some(rest) = rest.strip_prefix(' ')
    {
        m = rest;
    }
    if let Some(rest) = m.strip_prefix("is ") {
        m = rest;
    }
    let mut c = m.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// MARK: - Shell escaping

/// Backslash-escapes characters the shell treats specially (a leading `~` stays).
pub fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        if i == 0 && c == '~' {
            out.push(c);
            continue;
        }
        if "\\\t'\"$`!&;|()<>*?[]{}#~ ".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// MARK: - Palette matching

/// Lower is better; `None` when `query` is no subsequence of `title`. A substring match scores
/// its position, a scattered one 1000 plus its gaps (CommandPalette.swift).
pub fn fuzzy_score(title: &str, query: &str) -> Option<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let t = title.to_lowercase();
    if let Some(pos) = t.find(&q) {
        return Some(t[..pos].chars().count());
    }
    let tc: Vec<char> = t.chars().collect();
    let mut ti = 0;
    let mut gaps = 0;
    for qc in q.chars() {
        let found = tc[ti..].iter().position(|&c| c == qc)?;
        gaps += found;
        ti += found + 1;
    }
    Some(1000 + gaps)
}

// MARK: - URLs

/// Plain URLs in a line of text, as (start, end) char indices (end exclusive), trailing
/// punctuation stripped.
pub fn find_urls(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let stop = |c: char| c.is_whitespace() || "<>\"'`".contains(c);
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..chars.len().min(i + 8)].iter().collect();
        let scheme_len = ["https://", "http://", "ftp://", "file://", "mailto:"]
            .iter()
            .find(|s| rest.to_lowercase().starts_with(*s))
            .map(|s| s.chars().count());
        // A scheme only starts a URL at a word boundary.
        let boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if let Some(n) = scheme_len.filter(|_| boundary) {
            let mut end = i + n;
            while end < chars.len() && !stop(chars[end]) {
                end += 1;
            }
            while end > i + n && ".,;:!?)]}'\"".contains(chars[end - 1]) {
                end -= 1;
            }
            if end > i + n {
                out.push((i, end));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: u64) -> SplitNode {
        SplitNode::Leaf(PaneKey::local(id))
    }

    fn k(id: u64) -> PaneKey {
        PaneKey::local(id)
    }

    #[test]
    fn layout_round_trip_normalizes_left_and_up() {
        let stored = LayoutNode::Split {
            dir: SplitDir::Left,
            ratio: 0.3,
            first: Box::new(LayoutNode::local(1)),
            second: Box::new(LayoutNode::local(2)),
        };
        let node = SplitNode::from_layout(&stored, LOCAL);
        match &node {
            SplitNode::Split { axis, ratio, first, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert!((ratio - 0.7).abs() < 1e-9);
                assert_eq!(**first, leaf(2));
            }
            _ => panic!(),
        }
        let back = node.to_layout();
        assert!(matches!(back, LayoutNode::Split { dir: SplitDir::Right, .. }));
    }

    #[test]
    fn remote_leaves_keep_their_host() {
        let stored = LayoutNode::Pane {
            id: 4,
            host: Some("devbox".into()),
        };
        let node = SplitNode::from_layout(&stored, LOCAL);
        assert_eq!(node, SplitNode::Leaf(PaneKey::new("devbox", 4)));
        assert_eq!(node.to_layout(), stored);
    }

    #[test]
    fn split_and_remove() {
        let t = leaf(1).split(&k(1), k(2), Direction::Right).split(&k(2), k(3), Direction::Up);
        assert_eq!(t.panes(), vec![k(1), k(3), k(2)]);
        let t = t.remove(&k(3)).unwrap();
        assert_eq!(t.panes(), vec![k(1), k(2)]);
        assert!(leaf(1).remove(&k(1)).is_none());
    }

    #[test]
    fn divide_keeps_a_gap() {
        let (a, line, b) = divide(Rect::new(0.0, 0.0, 101.0, 50.0), Axis::Horizontal, 0.5);
        assert_eq!(a.w + line.w + b.w, 101.0);
        assert_eq!(line.x, a.w);
        assert_eq!(b.x, a.w + 1.0);
    }

    #[test]
    fn equalize_three_in_a_row() {
        let mut t = leaf(1)
            .split(&k(1), k(2), Direction::Right)
            .split(&k(2), k(3), Direction::Right);
        t.equalize();
        let mut frames = Vec::new();
        t.layout(Rect::new(0.0, 0.0, 302.0, 10.0), &mut frames, &mut Vec::new());
        let widths: Vec<f64> = frames.iter().map(|(_, r)| r.w).collect();
        assert!(widths.iter().all(|w| (w - 100.0).abs() <= 1.0), "{widths:?}");
    }

    #[test]
    fn resize_moves_the_nearest_matching_split() {
        let mut t = leaf(1)
            .split(&k(1), k(2), Direction::Right)
            .split(&k(2), k(3), Direction::Down);
        assert!(t.resize(&k(3), Axis::Horizontal, 0.05));
        match &t {
            SplitNode::Split { ratio, .. } => assert!((ratio - 0.55).abs() < 1e-9),
            _ => panic!(),
        }
        assert!(!leaf(1).resize(&k(1), Axis::Vertical, 0.05));
    }

    #[test]
    fn neighbor_prefers_overlap() {
        let frames = vec![
            (k(1), Rect::new(0.0, 0.0, 100.0, 100.0)),
            (k(2), Rect::new(101.0, 0.0, 100.0, 39.0)),
            (k(3), Rect::new(101.0, 40.0, 100.0, 60.0)),
        ];
        assert_eq!(neighbor(&frames, &k(3), Direction::Left), Some(k(1)));
        assert_eq!(neighbor(&frames, &k(2), Direction::Down), Some(k(3)));
        // Both overlap at the same distance: the nearer center wins.
        assert_eq!(neighbor(&frames, &k(1), Direction::Right), Some(k(3)));
        assert_eq!(neighbor(&frames, &k(1), Direction::Left), None);
    }

    #[test]
    fn names() {
        let n = workspace_name(&[], LOCAL);
        assert!(n.contains('-'));
        assert_eq!(workspace_name(&[], "devbox"), "devbox");
        assert!(workspace_name(&["devbox".into()], "devbox").starts_with("devbox-"));
    }

    #[test]
    fn paths_and_durations() {
        assert_eq!(abbreviate_path("/home/u/dev/personal/thurm", Some("/home/u")), "~/d/p/thurm");
        assert_eq!(abbreviate_path("/home/u/.config/thurm", Some("/home/u")), "~/.c/thurm");
        assert_eq!(abbreviate_path("/tmp", Some("/home/u")), "/tmp");
        assert_eq!(format_duration(42_000), "42s");
        assert_eq!(format_duration(185_000), "3m 05s");
        assert_eq!(format_duration(3_720_000), "1h 02m");
    }

    #[test]
    fn message_shortening() {
        assert_eq!(
            shorten_message("Claude needs your permission to use Bash", "Claude Code"),
            "Needs your permission to use Bash"
        );
        assert_eq!(shorten_message("Codex is waiting", "Codex"), "Waiting");
    }

    #[test]
    fn escaping() {
        assert_eq!(shell_escape("~/a b/(x).png"), "~/a\\ b/\\(x\\).png");
    }

    #[test]
    fn fuzzy() {
        assert_eq!(fuzzy_score("New Tab", ""), Some(0));
        assert_eq!(fuzzy_score("New Tab", "tab"), Some(4));
        assert_eq!(fuzzy_score("Split Right", "spr"), Some(1000 + 4));
        assert_eq!(fuzzy_score("Split Right", "xyz"), None);
    }

    #[test]
    fn urls() {
        let t = "see https://example.com/a?b=1). and mailto:me@x.org";
        let found = find_urls(t);
        let s: Vec<String> = found
            .iter()
            .map(|(a, b)| t.chars().skip(*a).take(b - a).collect())
            .collect();
        assert_eq!(s, vec!["https://example.com/a?b=1", "mailto:me@x.org"]);
        assert!(find_urls("xhttps://no").is_empty());
    }
}
