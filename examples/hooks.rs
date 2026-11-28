//! Behaviour as hooks: the loop decides nothing, the app composes what it wants.
//!
//! - an approval gate: `delete_note` asks on the terminal before it runs;
//! - an input hook: `/notes` is answered locally, without a model call;
//! - a context meter after every step;
//! - an overflow handler that explains instead of compacting (compaction is never automatic).
//!
//! ```sh
//! MANIFATURA_MODEL=llamacpp/qwen3.5-9b cargo run --example hooks -- "Add a note 'buy milk', then delete it"
//! ```

use std::io::{BufRead, Write};

use manifatura::{Agent, ErrorKind, Event, Hooks, Runtime, SessionId, Tool, hook, providers};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct Note {
    text: String,
}

/// Asks before tools named in `tools` run. A bundle is just a function returning `Hooks`.
fn approve(tools: &'static [&'static str]) -> Hooks {
    Hooks::new().on(move |e: hook::ToolCall, _cx| async move {
        if !tools.contains(&e.call.name.as_str()) {
            return hook::Decision::Run(e);
        }
        let question = format!("\nallow {} {}? [y/N] ", e.call.name, e.call.input);
        let yes = tokio::task::spawn_blocking(move || {
            eprint!("{question}");
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok();
            line.trim().eq_ignore_ascii_case("y")
        })
        .await
        .unwrap_or(false);
        if yes {
            hook::Decision::Run(e)
        } else {
            hook::Decision::Deny("the user declined".into())
        }
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let add = Tool::new("add_note", "Add a note.").run(|n: Note, cx| async move {
        let mut notes: Vec<String> = cx.state("notes").unwrap_or_default();
        notes.push(n.text);
        cx.set_state("notes", &notes);
        Ok(format!("{} notes", notes.len()))
    });
    let delete =
        Tool::new("delete_note", "Delete a note by its text.").run(|n: Note, cx| async move {
            let mut notes: Vec<String> = cx.state("notes").unwrap_or_default();
            notes.retain(|t| *t != n.text);
            cx.set_state("notes", &notes);
            Ok(format!("{} notes", notes.len()))
        });

    let agent = Agent::new("You keep the user's notes with your tools.")
        .tools([&add, &delete])
        .hooks(approve(&["delete_note"]))
        .on(|e: hook::Input, cx| async move {
            if e.text.trim() != "/notes" {
                return Some(e);
            }
            let notes: Vec<String> = cx.state("notes").unwrap_or_default();
            println!("notes: {notes:?}");
            None
        })
        .on(|e: hook::StepEnd, _cx| async move {
            if let Some(window) = e.context_window {
                eprintln!("[context {}/{window} tokens]", e.context_tokens);
            }
        })
        .on(|e: hook::ModelError, _cx| async move {
            if e.error.kind == ErrorKind::ContextOverflow {
                eprintln!("[context full: compact the session (Session::compact) and try again]");
            }
            hook::Recover::Fail
        });

    let rt = Runtime::builder().provider(providers::from_env()?).build();
    let session = rt.session(&agent, SessionId::new());
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Add a note 'buy milk', then delete it.".into()
    } else {
        prompt
    };

    for input in [prompt, "/notes".to_string()] {
        let mut turn = session.prompt(input).await?;
        while let Some(event) = turn.next_event().await {
            match event {
                Event::TextDelta { text, .. } => {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
                Event::ActionResult { name, output, .. } => eprintln!("\n{name} → {output}"),
                _ => {}
            }
        }
        turn.wait().await?;
        println!();
    }
    Ok(())
}
