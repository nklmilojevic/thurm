//! Portable tab layouts. Commands are argument arrays, never shell text.

use serde::{Deserialize, Serialize};

use crate::SplitDir;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutTemplate {
    pub version: u32,
    #[serde(default)]
    pub title: Option<String>,
    pub root: TemplateNode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TemplateNode {
    Pane {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        command: Option<Vec<String>>,
        #[serde(default)]
        hold: bool,
    },
    Split {
        dir: SplitDir,
        ratio: f64,
        first: Box<TemplateNode>,
        second: Box<TemplateNode>,
    },
}

impl LayoutTemplate {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported layout template version".into());
        }
        if self
            .title
            .as_ref()
            .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
        {
            return Err("layout title is too long or has control characters".into());
        }
        self.root.validate(0, &mut 0)
    }
}

impl TemplateNode {
    fn validate(&self, depth: usize, panes: &mut usize) -> Result<(), String> {
        if depth > 16 {
            return Err("layout exceeds 16 split levels".into());
        }
        match self {
            Self::Pane { cwd, command, .. } => {
                *panes += 1;
                if *panes > 64 {
                    return Err("layout exceeds 64 panes".into());
                }
                if cwd
                    .as_ref()
                    .is_some_and(|s| s.is_empty() || s.contains('\0'))
                {
                    return Err("invalid pane directory".into());
                }
                if let Some(argv) = command
                    && (argv.is_empty()
                        || argv[0].is_empty()
                        || argv.len() > 256
                        || argv.iter().any(|s| s.contains('\0'))
                        || argv.iter().map(String::len).sum::<usize>() > 65536)
                {
                    return Err("invalid pane command arguments".into());
                }
                Ok(())
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.05..=0.95).contains(ratio) {
                    return Err("split ratio must be between 0.05 and 0.95".into());
                }
                first.validate(depth + 1, panes)?;
                second.validate(depth + 1, panes)
            }
        }
    }
}

/// Check the layout before a desktop client receives it.
pub fn validate_tab(tab: &crate::TabLayout) -> Result<Vec<crate::PaneId>, String> {
    fn walk(
        node: &crate::LayoutNode,
        depth: usize,
        ids: &mut Vec<crate::PaneId>,
    ) -> Result<(), String> {
        if depth > 16 {
            return Err("layout exceeds 16 split levels".into());
        }
        match node {
            crate::LayoutNode::Pane { id, host } => {
                if host.is_some() || ids.contains(id) || ids.len() >= 64 {
                    return Err("layout must contain up to 64 unique local panes".into());
                }
                ids.push(*id);
            }
            crate::LayoutNode::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() || !(0.05..=0.95).contains(ratio) {
                    return Err("split ratio must be between 0.05 and 0.95".into());
                }
                walk(first, depth + 1, ids)?;
                walk(second, depth + 1, ids)?;
            }
        }
        Ok(())
    }
    let mut ids = Vec::new();
    walk(&tab.root, 0, &mut ids)?;
    if !ids.contains(&tab.focused) || tab.zoomed.is_some_and(|id| !ids.contains(&id)) {
        return Err("focused and zoomed panes must belong to the layout".into());
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_commands_and_ratios_before_creation() {
        for json in [
            r#"{"version":2,"root":{"type":"pane"}}"#,
            r#"{"version":1,"root":{"type":"pane","command":[]}}"#,
            r#"{"version":1,"root":{"type":"split","dir":"right","ratio":1,"first":{"type":"pane"},"second":{"type":"pane"}}}"#,
        ] {
            assert!(
                serde_json::from_str::<LayoutTemplate>(json)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn commands_keep_literal_arguments() {
        let t: LayoutTemplate = serde_json::from_str(
            r#"{"version":1,"root":{"type":"pane","command":["printf","%s","$(touch /tmp/no)"]}}"#,
        )
        .unwrap();
        t.validate().unwrap();
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("$(touch /tmp/no)"));
    }

    #[test]
    fn ui_layout_rejects_duplicate_panes_and_invalid_focus() {
        let mut tab = crate::TabLayout {
            title: None,
            root: crate::LayoutNode::local(7),
            focused: 7,
            zoomed: None,
            handoff: None,
        };
        assert_eq!(validate_tab(&tab).unwrap(), vec![7]);
        tab.focused = 8;
        assert!(validate_tab(&tab).is_err());
        tab.focused = 7;
        tab.root = crate::LayoutNode::Split {
            dir: SplitDir::Right,
            ratio: 0.5,
            first: Box::new(crate::LayoutNode::local(7)),
            second: Box::new(crate::LayoutNode::local(7)),
        };
        assert!(validate_tab(&tab).is_err());
    }
}
