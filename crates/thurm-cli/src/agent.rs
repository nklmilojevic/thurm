//! Public agent state commands.

use clap::{Args, Subcommand, ValueEnum};
use thurm_client::Client;
use thurm_proto::{AgentReport, AgentStatus, PaneId, Request, Response};

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Submit a prompt to an agent and optionally wait for that turn.
    Prompt(crate::agent_prompt::PromptArgs),
    /// Show the process, state source, and detection rules for a pane.
    Explain {
        #[arg(long)]
        pane: Option<PaneId>,
    },
    /// Report state from an agent process in a pane.
    Report(ReportArgs),
    /// End reporting for a process instance.
    Release(ReleaseArgs),
}

#[derive(Args)]
pub struct ReportArgs {
    #[arg(long)]
    pane: Option<PaneId>,
    /// Stable agent kind, such as claude or my-agent.
    #[arg(long)]
    agent: String,
    /// PID of the agent process, not the short-lived reporting command.
    #[arg(long)]
    owner_pid: u32,
    /// Unique ID for this process instance. Keep it for all reports.
    #[arg(long)]
    instance: String,
    /// Strictly increasing sequence number, starting at one.
    #[arg(long)]
    sequence: u64,
    #[arg(long, value_enum)]
    status: Status,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    message: Option<String>,
    /// JSON array with the executable and arguments to resume this session.
    #[arg(long)]
    resume_json: Option<String>,
}

#[derive(Args)]
pub struct ReleaseArgs {
    #[arg(long)]
    pane: Option<PaneId>,
    #[arg(long)]
    owner_pid: u32,
    #[arg(long)]
    instance: String,
    #[arg(long)]
    sequence: u64,
}

#[derive(Clone, ValueEnum)]
enum Status {
    Working,
    Idle,
    NeedsInput,
    Done,
}

pub fn run(c: &Client, action: AgentCmd, json: bool) -> crate::R {
    let request = match action {
        AgentCmd::Prompt(args) => return crate::agent_prompt::run(c, args, json),
        AgentCmd::Explain { pane } => Request::AgentExplain {
            pane: crate::current_pane(pane)?,
        },
        AgentCmd::Report(args) => {
            if crate::remote_mode() {
                return Err("run agent report on the host where the agent runs".into());
            }
            Request::AgentReport {
                pane: crate::current_pane(args.pane)?,
                report: AgentReport {
                    agent: args.agent,
                    owner_pid: args.owner_pid,
                    instance: args.instance,
                    sequence: args.sequence,
                    status: match args.status {
                        Status::Working => AgentStatus::Working,
                        Status::Idle => AgentStatus::Idle,
                        Status::NeedsInput => AgentStatus::NeedsInput,
                        Status::Done => AgentStatus::Done,
                    },
                    session_id: args.session_id,
                    message: args.message,
                    resume_argv: args
                        .resume_json
                        .map(|s| serde_json::from_str::<Vec<String>>(&s))
                        .transpose()?,
                },
            }
        }
        AgentCmd::Release(args) => {
            if crate::remote_mode() {
                return Err("run agent release on the host where the agent runs".into());
            }
            Request::AgentRelease {
                pane: crate::current_pane(args.pane)?,
                owner_pid: args.owner_pid,
                instance: args.instance,
                sequence: args.sequence,
            }
        }
    };
    match c.request(request)? {
        Response::AgentExplanation(e) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&e)?);
            } else {
                println!("pane {}: {}", e.pane, e.source);
                if let Some(state) = e.state {
                    println!("agent: {} ({:?})", state.kind, state.status);
                }
                if let Some(process) = e.foreground {
                    println!("process: {} (PID {})", process.name, process.pid);
                    println!("argv: {}", serde_json::to_string(&process.argv)?);
                }
                println!("idle: {} ms; threshold: {} ms", e.idle_ms, e.idle_after_ms);
                for rule in e.rules {
                    println!("rule: {rule}");
                }
                if let Some(instance) = e.report_instance {
                    println!(
                        "report: {instance}, owner PID {}, sequence {}",
                        e.report_owner_pid.unwrap_or(0),
                        e.report_sequence.unwrap_or(0)
                    );
                }
                if !e.screen_tail.is_empty() {
                    println!("screen evidence:\n{}", e.screen_tail);
                }
            }
        }
        Response::Ok => {
            if json {
                println!("{{\"ok\":true}}");
            }
        }
        other => return Err(format!("unexpected agent response: {other:?}").into()),
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn report_requires_a_process_identity_sequence_and_status() {
        assert!(
            crate::Cli::try_parse_from([
                "thurm",
                "agent",
                "report",
                "--agent",
                "example",
                "--owner-pid",
                "42",
                "--instance",
                "one",
                "--sequence",
                "1",
                "--status",
                "needs-input",
            ])
            .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from([
                "thurm", "agent", "report", "--agent", "example", "--status", "idle",
            ])
            .is_err()
        );
    }

    #[test]
    fn explain_and_release_are_public_commands() {
        assert!(
            crate::Cli::try_parse_from(["thurm", "--json", "agent", "explain", "--pane", "7",])
                .is_ok()
        );
        assert!(
            crate::Cli::try_parse_from([
                "thurm",
                "agent",
                "release",
                "--pane",
                "7",
                "--owner-pid",
                "42",
                "--instance",
                "one",
                "--sequence",
                "2",
            ])
            .is_ok()
        );
    }
}
