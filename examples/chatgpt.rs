//! Sign in with ChatGPT and run an agent on the user's plan, with one tool.
//!
//! ```sh
//! cargo run --example chatgpt -- login          # opens the sign-in URL
//! cargo run --example chatgpt -- models
//! cargo run --example chatgpt -- ask "What time is it?"
//! cargo run --example chatgpt -- logout
//! ```

use std::io::Write;

use manifatura::chatgpt::{ChatGpt, USAGE_URL};
use manifatura::{Agent, Event, Runtime, SessionId, Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Clock {}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let chatgpt = ChatGpt::new(
        "manifatura-example",
        ChatGpt::default_path("manifatura-example"),
    );
    match args.first().map(String::as_str) {
        Some("login") => {
            let login = chatgpt.login().await?;
            println!("Open this URL to sign in:\n\n{}\n", login.url());
            let account = login.finish().await?;
            println!(
                "Signed in as {}",
                account.email.as_deref().unwrap_or(&account.subject)
            );
            if !account.plan_usage {
                println!("Plan usage was not allowed; sign in again and allow it.");
            }
        }
        Some("logout") => {
            chatgpt.logout(false).await?;
            println!("Signed out.");
        }
        Some("models") => {
            for m in chatgpt.models().await? {
                println!("{}\t{}", m.slug, m.display_name);
            }
        }
        Some("ask") => {
            let prompt = args[1..].join(" ");
            let model = match std::env::var("MODEL") {
                Ok(m) => m,
                Err(_) => chatgpt
                    .models()
                    .await?
                    .first()
                    .ok_or("no models listed")?
                    .slug
                    .clone(),
            };
            let clock = Tool::new("clock", "The current Unix time in seconds.").run(
                |_: Clock, _cx| async move {
                    let t = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(ToolError::msg)?;
                    Ok(t.as_secs().to_string())
                },
            );
            let agent = Agent::new("Be brief.").model(model.clone()).tools([&clock]);
            let rt = Runtime::builder().provider(chatgpt.provider()).build();
            let session = rt.session(&agent, SessionId::new());
            eprintln!("Using ChatGPT plan ({model}) · Manage usage: {USAGE_URL}");
            let mut turn = session.prompt(prompt).await?;
            while let Some(e) = turn.next_event().await {
                match e {
                    Event::TextDelta { text, .. } => {
                        print!("{text}");
                        std::io::stdout().flush()?;
                    }
                    Event::ActionStarted { name, .. } => eprintln!("\n[{name}]"),
                    _ => {}
                }
            }
            println!();
            turn.wait().await?;
        }
        _ => eprintln!("usage: chatgpt login | models | ask <prompt> | logout"),
    }
    Ok(())
}
