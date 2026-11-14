//! Commands over the extension API: `thurm action`.

use std::process::ExitCode;

use clap::Subcommand;
use serde_json::{Value, json};
use thurm_client::Client;
use thurm_proto::ext::{self, method};
use thurm_proto::{ENV_PANE_ID, PaneId, Request, Response};

type R = Result<ExitCode, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum ActionCmd {
    /// List the actions (the config's commands, what plugins and the app provide) and their keys.
    List,
    /// Run an action, for a pane (default: this one, else the focused one), and print what it
    /// returned. ARGS is a JSON value: `thurm action run git.checkout '{"branch":"main"}'`.
    Run {
        name: String,
        args: Option<String>,
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
}

/// Calls `method` of the daemon's extension API.
pub fn call(c: &Client, method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error>> {
    match c.request(Request::Ext {
        method: method.into(),
        params: params.to_string(),
    })? {
        Response::Text(t) => Ok(serde_json::from_str(&t)?),
        other => Err(format!("unexpected response {other:?}").into()),
    }
}

pub fn action(c: &Client, cmd: ActionCmd, json: bool) -> R {
    match cmd {
        ActionCmd::List => {
            let list: ext::Actions =
                serde_json::from_value(call(c, method::ACTIONS_LIST, Value::Null)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            let keys = |name: &str| {
                list.bindings
                    .iter()
                    .filter(|b| b.action == name)
                    .map(|b| b.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let width = list
                .actions
                .iter()
                .map(|a| a.name.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(4, 40);
            println!("{:<width$}  {:<16} {:<14} TITLE", "NAME", "SOURCE", "KEY");
            for a in &list.actions {
                println!(
                    "{:<width$}  {:<16} {:<14} {}",
                    a.name,
                    a.source,
                    keys(&a.name),
                    a.title()
                );
            }
        }
        ActionCmd::Run { name, args, pane } => {
            let args: Value = match args.as_deref() {
                None => Value::Null,
                // A bare word is a string, as `thurm set` takes them.
                Some(a) => serde_json::from_str(a).unwrap_or_else(|_| Value::String(a.into())),
            };
            let pane = pane.or_else(|| std::env::var(ENV_PANE_ID).ok()?.parse().ok());
            let result = call(
                c,
                method::ACTIONS_RUN,
                json!({ "name": name, "args": args, "pane": pane }),
            )?;
            match result {
                Value::Null => {}
                Value::String(s) if !json => println!("{s}"),
                other => println!("{}", serde_json::to_string_pretty(&other)?),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
