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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => println!("{}", version_string()),
        Some("spike") => spike().await?,
        _ => {
            println!("actmaton (M0 seed)");
            println!("usage: actmaton [--version] | spike");
            println!("  --version   print the actmaton version");
            println!("  spike       run the Copilot SDK hello-world spike");
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

    println!("reply: {reply:?}");

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
