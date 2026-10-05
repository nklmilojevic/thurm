use std::io::Read;
use std::process::ExitCode;

use clap::Args;
use thurm_client::Client;
use thurm_proto::{AgentPromptOutcome, PaneId, Request, Response};

#[derive(Args)]
pub struct PromptArgs {
    /// Target pane. Defaults to THURM_PANE_ID.
    #[arg(long)]
    pane: Option<PaneId>,
    /// Wait for this agent turn to finish or request input.
    #[arg(long)]
    wait: bool,
    /// Maximum wait in seconds, from 1 to 86400.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout: u64,
    /// Prompt text. Use - to read from standard input.
    #[arg(required = true)]
    text: Vec<String>,
}

pub fn run(client: &Client, args: PromptArgs, json: bool) -> crate::R {
    let pane = crate::current_pane(args.pane)?;
    let text = if args.text == ["-"] {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        args.text.join(" ")
    };
    let response = client.request(Request::AgentPrompt {
        pane,
        text,
        wait: args.wait,
        timeout_ms: args.timeout * 1000,
    })?;
    let Response::AgentPrompt(outcome) = response else {
        return Err(format!("unexpected response {response:?}").into());
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"pane": pane, "outcome": format!("{outcome:?}")})
        );
    } else {
        println!(
            "{}",
            match outcome {
                AgentPromptOutcome::Submitted => "Prompt submitted.",
                AgentPromptOutcome::Completed => "Agent turn completed.",
                AgentPromptOutcome::NeedsInput => "Agent needs input.",
                AgentPromptOutcome::Timeout => "Agent prompt wait timed out.",
                AgentPromptOutcome::Exited => "Agent pane exited.",
            }
        );
    }
    Ok(match outcome {
        AgentPromptOutcome::Submitted | AgentPromptOutcome::Completed => ExitCode::SUCCESS,
        AgentPromptOutcome::NeedsInput => ExitCode::from(3),
        AgentPromptOutcome::Timeout => ExitCode::from(124),
        AgentPromptOutcome::Exited => ExitCode::from(2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        prompt: PromptArgs,
    }

    #[test]
    fn wait_option_after_prompt_is_an_option() {
        let args = TestCli::try_parse_from(["prompt", "--pane", "12", "Check it", "--wait"])
            .unwrap()
            .prompt;
        assert!(args.wait);
        assert_eq!(args.pane, Some(12));
        assert_eq!(args.text, ["Check it"]);
    }

    #[test]
    fn stdin_and_timeout_bounds() {
        assert_eq!(
            TestCli::try_parse_from(["prompt", "-"])
                .unwrap()
                .prompt
                .text,
            ["-"]
        );
        for timeout in ["0", "86401", "inf", "-1"] {
            assert!(TestCli::try_parse_from(["prompt", "--timeout", timeout, "text"]).is_err());
        }
    }
}
