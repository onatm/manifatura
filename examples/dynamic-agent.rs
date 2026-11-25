//! A dynamic agent: a `Step` hook runs before every model call, and the `refund` tool appears
//! only after a tool has set session state. The session is kept in a JSONL file you can inspect
//! with `jq`.
//!
//! ```sh
//! MANIFATURA_MODEL=llamacpp/qwen3.5-9b cargo run --example dynamic-agent
//! ```

use std::io::Write;

use manifatura::store::JsonlStore;
use manifatura::{Agent, Event, Runtime, SessionId, Tool, hook, models};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Escalate {
    /// Why the case needs a supervisor.
    reason: String,
}

#[derive(Deserialize, JsonSchema)]
struct Refund {
    order_id: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let escalate = Tool::new(
        "escalate",
        "Escalate the case to a supervisor. Required before any refund.",
    )
    .run(|i: Escalate, cx| async move {
        cx.set_state("escalated", true);
        Ok(format!(
            "Escalated ({}). A supervisor approved refunds for this case.",
            i.reason
        ))
    });
    let refund = Tool::new("refund", "Refund an order in full.")
        .run(|i: Refund, _cx| async move { Ok(format!("Refunded order {}.", i.order_id)) });

    let agent = Agent::new(
        "You handle support for an online shop. You cannot refund until the case is \
         escalated; after escalation a refund tool becomes available.",
    )
    .name("support")
    .tool(&escalate)
    .on(move |step: hook::Step, cx| {
        let refund = refund.clone();
        async move {
            if cx.state("escalated").unwrap_or(false) {
                eprintln!("\n[step {}: refund available]", cx.step().unwrap_or(0));
                step.tool(&refund)
            } else {
                step
            }
        }
    });

    let dir = std::env::temp_dir().join("manifatura-dynamic-agent");
    let store = JsonlStore::new(&dir)?;
    let id = SessionId::new();
    let path = store.path(&id);
    let rt = Runtime::builder()
        .provider(models::from_env()?)
        .store(store)
        .build();

    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Order A-1001 arrived broken. I want my money back.".into()
    } else {
        prompt
    };
    let mut turn = rt.session(&agent, id).prompt(prompt).await?;

    while let Some(event) = turn.next_event().await {
        match event {
            Event::TextDelta { text, .. } => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            Event::ActionStarted { name, input, .. } => eprintln!("→ {name} {input}"),
            Event::ActionResult { output, .. } => eprintln!("← {output}"),
            _ => {}
        }
    }
    turn.wait().await?;
    println!();
    eprintln!("session log: {}", path.display());
    Ok(())
}
