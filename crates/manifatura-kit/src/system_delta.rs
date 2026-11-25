//! Prompt-cache friendly loadout changes.

use std::collections::BTreeSet;

use manifatura_agent::{Hooks, hook};
use serde::{Deserialize, Serialize};

const KEY: &str = "kit.system_delta";

/// What the model was last told it has.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Declared {
    /// The first system prompt: sent unchanged from then on.
    system: String,
    instructions: String,
    tools: BTreeSet<String>,
}

/// Keeps the first system prompt of a session fixed and tells the model about later changes
/// (new instructions, tools added or removed) in a note, so the cached prompt prefix stays
/// valid. Add it after every hook that changes the instructions or tools. What was declared is
/// kept in session state, so a reopened session continues where it left off.
pub fn system_delta() -> Hooks {
    Hooks::new().on(|mut e: hook::Step, cx| async move {
        let tools: BTreeSet<String> = e
            .tools
            .iter()
            .filter(|t| !t.is_deferred())
            .map(|t| t.name().to_string())
            .collect();
        let Some(mut d) = cx.state::<Declared>(KEY) else {
            cx.set_state(
                KEY,
                Declared {
                    system: e.instructions.clone(),
                    instructions: e.instructions.clone(),
                    tools,
                },
            );
            return e;
        };
        let mut note = Vec::new();
        if e.instructions != d.instructions {
            note.push(format!(
                "Your instructions changed. They now read:\n\n{}",
                e.instructions
            ));
        }
        let added: Vec<&str> = tools.difference(&d.tools).map(String::as_str).collect();
        let removed: Vec<&str> = d.tools.difference(&tools).map(String::as_str).collect();
        if !added.is_empty() {
            note.push(format!("Tools now available: {}.", added.join(", ")));
        }
        if !removed.is_empty() {
            note.push(format!(
                "Tools no longer available: {}.",
                removed.join(", ")
            ));
        }
        if !note.is_empty() {
            cx.note(note.join("\n\n"));
            d.instructions = e.instructions.clone();
            d.tools = tools;
            cx.set_state(KEY, &d);
        }
        e.instructions = d.system;
        e
    })
}
