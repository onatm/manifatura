//! `update_plan`: the model keeps a short plan for work with several steps, in the shape OpenAI's models are
//! trained on (Codex's). The screen shows it as a checklist; the tool itself only checks it.

use manifatura::{Tool, ToolCx, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct PlanInput {
    /// Why the plan changed, if it did.
    explanation: Option<String>,
    /// The steps, in order.
    plan: Vec<Step>,
}

#[derive(Deserialize, JsonSchema)]
struct Step {
    /// What the step does, in a few words.
    step: String,
    status: Status,
}

#[derive(Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Completed,
}

/// The `update_plan` tool.
pub(crate) fn update_plan() -> Tool {
    Tool::new(
        "update_plan",
        "Updates the task plan. Give an optional explanation and the list of steps, each with a status. At most \
         one step can be in_progress at a time.",
    )
    .guideline(
        "For work with several steps, keep a short plan with `update_plan`: mark a step in_progress before \
         starting it and completed once it is done. Skip it for simple tasks.",
    )
    .run(|i: PlanInput, _cx: ToolCx| async move {
        let _ = i.explanation;
        if i.plan.iter().filter(|s| s.status == Status::InProgress).count() > 1 {
            return Err(ToolError::msg("at most one step can be in_progress"));
        }
        if i.plan.iter().any(|s| s.step.trim().is_empty()) {
            return Err(ToolError::msg("every step needs a description"));
        }
        Ok("Plan updated")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_plan_has_at_most_one_step_in_progress() {
        let tool = update_plan();
        let call = |plan| tool.call(json!({ "plan": plan }), ToolCx::detached("c"));
        let r = call(
            json!([{"step": "a", "status": "completed"}, {"step": "b", "status": "in_progress"}]),
        )
        .await;
        assert!(!r.is_error, "{}", r.output);
        let r = call(
            json!([{"step": "a", "status": "in_progress"}, {"step": "b", "status": "in_progress"}]),
        )
        .await;
        assert!(r.is_error);
        let r = call(json!([{"step": "a", "status": "doing"}])).await;
        assert!(r.is_error, "an unknown status");
    }
}
