use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

use clap::Subcommand;
use thurm_client::Client;
use thurm_proto::template::{LayoutTemplate, TemplateNode};
use thurm_proto::{CreatePane, Layout, LayoutNode, PaneId, PaneInfo, Request, Response, TabLayout};

#[derive(Subcommand)]
pub enum LayoutCmd {
    /// Export the tab that contains this pane. Commands are not copied.
    Export {
        #[arg(long)]
        pane: Option<PaneId>,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Create a workspace from a template. Commands in the file will run.
    Apply { file: PathBuf },
}

pub fn run(c: &Client, action: Option<LayoutCmd>) -> super::R {
    match action {
        None => {
            let Response::Layout(layout) = c.request(Request::GetLayout)? else {
                return Err("unexpected layout response".into());
            };
            let value: serde_json::Value =
                serde_json::from_str(layout.as_deref().unwrap_or("null"))?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Some(LayoutCmd::Export { pane, output }) => {
            let pane = super::current_pane(pane)?;
            let Response::Layout(Some(json)) = c.request(Request::GetLayout)? else {
                return Err("no saved layout is available".into());
            };
            let layout: Layout = serde_json::from_str(&json)?;
            let tab = layout
                .windows
                .iter()
                .flat_map(|w| &w.tabs)
                .chain(layout.workspaces.iter().flat_map(|w| &w.tabs))
                .chain(&layout.quick)
                .find(|t| {
                    let mut ids = Vec::new();
                    t.root.panes(&mut ids);
                    ids.contains(&pane)
                })
                .ok_or("pane is not in the saved layout")?;
            let Response::Panes(panes) = c.request(Request::ListPanes)? else {
                return Err("unexpected pane list response".into());
            };
            let infos: HashMap<_, _> = panes.into_iter().map(|p| (p.id, p)).collect();
            let template = LayoutTemplate {
                version: 1,
                title: tab.title.clone(),
                root: export_node(&tab.root, &infos)?,
            };
            template.validate()?;
            let json = serde_json::to_string_pretty(&template)? + "\n";
            match output {
                Some(path) => std::fs::write(path, json)?,
                None => print!("{json}"),
            }
        }
        Some(LayoutCmd::Apply { file }) => {
            let mut json = String::new();
            std::fs::File::open(file)?
                .take(1024 * 1024 + 1)
                .read_to_string(&mut json)?;
            if json.len() > 1024 * 1024 {
                return Err("layout template exceeds 1 MiB".into());
            }
            let template: LayoutTemplate = serde_json::from_str(&json)?;
            template.validate()?;
            c.request(Request::CheckUi)?;
            let mut created = Vec::new();
            let mut rollback = true;
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let root = create_node(c, &template.root, &mut created)?;
                let tab = TabLayout {
                    title: template.title,
                    root,
                    focused: created[0],
                    zoomed: None,
                    handoff: None,
                };
                // Once sent, only a confirmed pre-commit failure permits cleanup.
                rollback = false;
                let Response::LayoutResult(result) = c.request(Request::ApplyLayout {
                    json: serde_json::to_string(&tab)?,
                    timeout_ms: 10_000,
                })?
                else {
                    return Err("unexpected layout result".into());
                };
                rollback = !result.committed;
                if let Some(error) = result.error {
                    return Err(error.into());
                }
                if !result.committed {
                    return Err("the desktop did not commit the layout".into());
                }
                println!("{}", serde_json::json!({"panes": created}));
                Ok(())
            })();
            if let Err(error) = result {
                if rollback {
                    for pane in created {
                        let _ = c.request(Request::ClosePane { pane });
                    }
                } else {
                    eprintln!(
                        "layout state is uncertain; panes kept: {}",
                        serde_json::json!(created)
                    );
                }
                return Err(error);
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn export_node(
    node: &LayoutNode,
    infos: &HashMap<PaneId, PaneInfo>,
) -> Result<TemplateNode, String> {
    Ok(match node {
        LayoutNode::Pane { id, host: None } => {
            let info = infos.get(id).ok_or("a layout pane no longer exists")?;
            TemplateNode::Pane {
                cwd: info.cwd.clone(),
                command: None,
                hold: false,
            }
        }
        LayoutNode::Pane { host: Some(_), .. } => {
            return Err("export the layout through its remote daemon".into());
        }
        LayoutNode::Split {
            dir,
            ratio,
            first,
            second,
        } => TemplateNode::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(export_node(first, infos)?),
            second: Box::new(export_node(second, infos)?),
        },
    })
}

fn create_node(
    c: &Client,
    node: &TemplateNode,
    created: &mut Vec<PaneId>,
) -> Result<LayoutNode, Box<dyn std::error::Error>> {
    Ok(match node {
        TemplateNode::Pane { cwd, command, hold } => {
            let Response::PaneCreated { pane } = c.request(Request::CreatePane(CreatePane {
                cwd: cwd.clone(),
                command: command.clone(),
                hold: *hold,
                ..Default::default()
            }))?
            else {
                return Err("unexpected pane creation response".into());
            };
            created.push(pane);
            LayoutNode::local(pane)
        }
        TemplateNode::Split {
            dir,
            ratio,
            first,
            second,
        } => LayoutNode::Split {
            dir: *dir,
            ratio: *ratio,
            first: Box::new(create_node(c, first, created)?),
            second: Box::new(create_node(c, second, created)?),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_keeps_layout_and_directory_without_process_arguments() {
        let infos = HashMap::from([(
            3,
            PaneInfo {
                id: 3,
                cwd: Some("/tmp/project".into()),
                ..Default::default()
            },
        )]);
        let node = export_node(&LayoutNode::local(3), &infos).unwrap();
        let TemplateNode::Pane { cwd, command, .. } = node else {
            panic!("expected pane")
        };
        assert_eq!(cwd.as_deref(), Some("/tmp/project"));
        assert!(command.is_none());
        assert!(export_node(&LayoutNode::local(4), &infos).is_err());
    }
}
