//! A typed tool without macros: the input schema comes from `JsonSchema`, the input is
//! deserialised before the closure runs, and `?` works on any error.
//!
//! ```sh
//! MANIFATURA_MODEL=llamacpp/qwen3.5-9b cargo run --example typed-tool -- "Where is order A-1002?"
//! ```

use std::io::Write;

use manifatura::{Agent, Event, Json, Runtime, SessionId, Tool, ToolError, models};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, JsonSchema)]
struct Lookup {
    /// The order id, e.g. "A-1001".
    order_id: String,
}

#[derive(Serialize)]
struct Order {
    id: &'static str,
    status: &'static str,
    items: &'static [&'static str],
}

const ORDERS: &[Order] = &[
    Order {
        id: "A-1001",
        status: "delivered",
        items: &["kettle"],
    },
    Order {
        id: "A-1002",
        status: "in transit, arriving Thursday",
        items: &["lamp", "bulbs"],
    },
];

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let lookup =
        Tool::new("lookup_order", "Look up one order by id.").run(|i: Lookup, _cx| async move {
            let order = ORDERS
                .iter()
                .find(|o| o.id.eq_ignore_ascii_case(&i.order_id))
                .ok_or_else(|| ToolError::msg(format!("no order with id {}", i.order_id)))?;
            Ok(Json(order))
        });

    let agent =
        Agent::new("You answer questions about orders. Look orders up; never guess.").tool(&lookup);
    let rt = Runtime::builder().provider(models::from_env()?).build();

    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Where is order A-1002 and what's in it?".into()
    } else {
        prompt
    };
    let mut turn = rt.session(&agent, SessionId::new()).prompt(prompt).await?;

    while let Some(event) = turn.next_event().await {
        match event {
            Event::TextDelta { text, .. } => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            Event::ActionStarted { name, input, .. } => eprintln!("→ {name} {input}"),
            Event::ActionResult {
                output, is_error, ..
            } => {
                eprintln!("{} {output}", if is_error { "✗" } else { "←" })
            }
            _ => {}
        }
    }
    turn.wait().await?;
    println!();
    Ok(())
}
