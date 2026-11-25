//! The smallest Manifatura app: one static agent, providers from the environment, the reply
//! streamed to stdout.
//!
//! ```sh
//! MANIFATURA_MODEL=llamacpp/gemma cargo run --example hello -- "Why is the sky blue?"
//! MANIFATURA_MODEL=llamacpp/qwen3.5-9b cargo run --example hello
//! ```

use std::io::Write;

use manifatura::{Agent, Event, Runtime, SessionId, models};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Say hello in five words.".into()
    } else {
        prompt
    };

    let agent = Agent::new("You are concise.");
    let rt = Runtime::builder().provider(models::from_env()?).build();
    let mut turn = rt.session(&agent, SessionId::new()).prompt(prompt).await?;

    while let Some(event) = turn.next_event().await {
        if let Event::TextDelta { text, .. } = event {
            print!("{text}");
            std::io::stdout().flush()?;
        }
    }
    let reply = turn.wait().await?;
    println!();
    eprintln!(
        "tokens: {} in, {} out",
        reply.usage.input, reply.usage.output
    );
    Ok(())
}
