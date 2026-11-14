//! actmaton — M0 Seed spike.
//!
//! Minimal hello-world against the GitHub Copilot SDK for Rust:
//! start a client, create a session, send one prompt, print the reply.
//! This proves the SDK wiring compiles and starts; a runtime auth
//! failure is expected until a Copilot sign-in exists on the machine.

use std::sync::Arc;
use std::time::Duration;

use github_copilot_sdk::handler::ApproveAllHandler;
use github_copilot_sdk::{Client, ClientOptions, MessageOptions, SessionConfig};

mod propose;
mod run_loop;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => println!("{}", version_string()),
        Some("spike") => spike().await?,
        Some("run") => match args[2..].iter().find(|a| !a.starts_with("--")) {
            Some(issue_ref) => {
                let dry_run = args[2..].iter().any(|a| a == "--dry-run");
                run_loop::run(issue_ref, dry_run).await?
            }
            None => {
                eprintln!("usage: actmaton run <issue-number-or-url> [--dry-run]");
                std::process::exit(2);
            }
        },
        Some("propose") => propose::propose().await?,
        _ => {
            println!("actmaton (M0 seed)");
            println!("usage: actmaton --version          # print the actmaton version");
            println!("       actmaton spike              # run the Copilot SDK hello-world spike");
            println!("       actmaton run <issue|url>    # run the M0 loop for one GitHub issue");
            println!("       actmaton run <issue|url> --dry-run  # print prompt and permission policy; no model call");
            println!("       actmaton propose            # draft the next task as an actmaton-proposal issue");
        }
    }
    Ok(())
}

fn version_string() -> String {
    format!("actmaton {}", env!("CARGO_PKG_VERSION"))
}

async fn spike() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::start(ClientOptions::default()).await?;

    let session = client
        .create_session(
            SessionConfig::default().with_permission_handler(Arc::new(ApproveAllHandler)),
        )
        .await?;

    let reply = session
        .send_and_wait(
            MessageOptions::new("Reply with a short greeting")
                .with_wait_timeout(Duration::from_secs(120)),
        )
        .await?;

    match reply.as_ref().and_then(run_loop::final_message_text) {
        Some(text) => println!("reply: {text}"),
        None => println!("reply: (no message)"),
    }

    session.disconnect().await?;
    client.stop().await.ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_string_matches_cargo() {
        assert_eq!(super::version_string(), "actmaton 0.1.0");
    }

    #[test]
    fn trivial_arithmetic() {
        assert_eq!(2 + 2, 4);
    }
}
