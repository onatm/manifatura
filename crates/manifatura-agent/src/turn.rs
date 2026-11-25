//! The loop (pi's structure): stream, run tools, repeat. Every decision is a hook's; see
//! [`hook`](crate::hook) for the points and what each returns.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures_util::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::compact::{CompactCx, CompactPlan, CompactReport, Compactor};
use crate::context::{Live, prompt_tokens};
use crate::event::{Event, Outcome};
use crate::hook::{self, CompactDecision, Decision, Next, Recover};
use crate::hooks::HookCx;
use crate::provider::Abort;
use crate::record::{Entry, Record, StoreError};
use crate::runtime::{Error, Reply, Runtime, Shared};
use crate::tool::{Tool, ToolCx, ToolResult, Writes};
use crate::types::{
    Content, Message, ModelRequest, Response, Role, StopReason, StreamEvent, ToolCall, ToolSpec,
    Usage,
};

/// Failed model calls a turn may retry, however often hooks ask: a guard against a hook that
/// always says `Retry`.
const MAX_RETRIES: usize = 8;

const CUT_OFF: &str = "Not run: the output token limit cut this message off, so the call may be \
incomplete. Retry with a smaller call.";

pub(crate) struct TurnJob {
    pub id: String,
    pub input: String,
    pub abort: Abort,
    pub done: oneshot::Sender<Result<Reply, Error>>,
}

/// Why a turn stopped early.
enum Stop {
    Failed(Error),
    Aborted,
}

impl Stop {
    fn failed(reason: impl Into<String>) -> Self {
        Stop::Failed(Error::Failed(reason.into()))
    }
}

impl From<StoreError> for Stop {
    fn from(e: StoreError) -> Self {
        Stop::Failed(Error::Store(e))
    }
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        match e {
            Error::Aborted => Stop::Aborted,
            e => Stop::Failed(e),
        }
    }
}

pub(crate) async fn run(
    shared: &Arc<Shared>,
    live: &mut Live,
    job: &TurnJob,
) -> Result<Reply, Error> {
    let turn = job.id.as_str();
    let cx = shared.cx(live, Some(turn), None, &job.abort);
    shared
        .emit(&cx, Event::TurnStarted { turn: turn.into() })
        .await;
    let mut usage = Usage::default();
    let (outcome, error, last) = match body(shared, live, job, &mut usage).await {
        Ok(last) => (Outcome::Completed, None, last),
        Err(Stop::Failed(e)) => (Outcome::Failed, Some(e), None),
        Err(Stop::Aborted) => (Outcome::Aborted, None, None),
    };
    let cx = shared.cx(live, Some(turn), None, &job.abort);
    shared
        .emit(
            &cx,
            Event::TurnCompleted {
                turn: turn.into(),
                outcome,
                error: error.as_ref().map(Error::reason),
                usage,
            },
        )
        .await;
    match (outcome, error) {
        (_, Some(e)) => Err(e),
        (Outcome::Aborted, None) => Err(Error::Aborted),
        _ => Ok(Reply {
            turn: turn.into(),
            text: last.as_ref().map(Message::text).unwrap_or_default(),
            message: last,
            usage,
        }),
    }
}

async fn body(
    shared: &Arc<Shared>,
    live: &mut Live,
    job: &TurnJob,
    usage: &mut Usage,
) -> Result<Option<Message>, Stop> {
    let turn = job.id.as_str();
    let abort = &job.abort;
    let agent = &shared.agent;

    let cx = shared.cx(live, Some(turn), None, abort);
    let input = shared
        .hooks
        .run(
            hook::Input {
                text: job.input.clone(),
            },
            &cx,
        )
        .await;
    flush(shared, live, Some(turn), abort).await?;
    let Some(input) = input else {
        return Ok(None);
    };
    push_user(shared, live, vec![input.text]).await?;
    let cx = shared.cx(live, Some(turn), None, abort);
    shared.hooks.run(hook::TurnStart, &cx).await;

    let mut step = 0;
    let mut failures = 0;
    loop {
        flush(shared, live, Some(turn), abort).await?;
        if abort.is_aborted() {
            return Err(Stop::Aborted);
        }
        let Some(model) = shared.model(live) else {
            return Err(Stop::failed(
                "no model configured: use Agent::model or RuntimeBuilder::model",
            ));
        };
        if step >= agent.max_steps {
            return Err(Stop::failed(format!(
                "stopped after {} model calls without finishing",
                agent.max_steps
            )));
        }
        step += 1;

        let cx = shared.cx(live, Some(turn), Some(step), abort);
        let s = shared
            .hooks
            .run(
                hook::Step {
                    instructions: agent.instructions.clone(),
                    tools: agent.tools.clone(),
                },
                &cx,
            )
            .await;
        // Notes and state a `Step` hook wrote reach this step's request.
        flush(shared, live, Some(turn), abort).await?;
        let mut cx = shared.cx(live, Some(turn), Some(step), abort);
        cx.tools = Arc::from(s.tools.as_slice());
        let specs: Vec<ToolSpec> = s
            .tools
            .iter()
            .filter(|t| !t.is_deferred())
            .map(|t| t.spec().clone())
            .collect();
        let context = shared
            .hooks
            .run(
                hook::Context {
                    messages: live.messages.clone(),
                },
                &cx,
            )
            .await;
        let request = ModelRequest {
            model,
            system: s.instructions.clone(),
            messages: context.messages,
            tools: specs,
            max_tokens: agent.max_tokens,
            reasoning: live.reasoning.or(agent.reasoning),
        };
        let request = shared
            .hooks
            .run(hook::Request { request }, &cx)
            .await
            .request;
        let window = shared.window(&request.model);
        // Measured once: the estimate after the response needs it only if the provider
        // reports no usage.
        let prompt = prompt_tokens(&request.system, &request.tools);
        shared
            .emit(
                &cx,
                Event::StepStarted {
                    turn: turn.into(),
                    step,
                    context_tokens: live.estimate(prompt),
                    context_window: window,
                },
            )
            .await;

        let Some(resp) = stream(shared, &cx, turn, request, abort).await else {
            return Err(Stop::Aborted);
        };
        *usage += resp.usage;
        match &resp.stop {
            StopReason::Error(e) => {
                failures += 1;
                let r = shared
                    .hooks
                    .run(
                        hook::ModelError {
                            error: e.clone(),
                            attempt: failures,
                        },
                        &cx,
                    )
                    .await;
                if r == Recover::Retry && failures <= MAX_RETRIES {
                    // The retried step does not count; compaction a hook asked for runs first.
                    step -= 1;
                    continue;
                }
                return Err(Stop::Failed(Error::Model(e.clone())));
            }
            StopReason::Aborted => {
                let text: Vec<Content> = resp
                    .message
                    .content
                    .into_iter()
                    .filter(|c| matches!(c, Content::Text { .. }))
                    .collect();
                if !text.is_empty() {
                    let message = Message::new(Role::Assistant, text);
                    append(shared, live, vec![Record::Message { message }]).await?;
                }
                return Err(Stop::Aborted);
            }
            _ => {}
        }

        let resp = shared
            .hooks
            .run(
                hook::Response {
                    message: resp.message,
                    usage: resp.usage,
                    stop: resp.stop,
                },
                &cx,
            )
            .await;
        let message = resp.message;
        append(
            shared,
            live,
            vec![Record::Message {
                message: message.clone(),
            }],
        )
        .await?;
        let measured =
            resp.usage.input + resp.usage.cache_read + resp.usage.cache_write + resp.usage.output;
        if measured > 0 {
            live.measured = Some((measured, live.messages.len()));
        }
        shared
            .emit(
                &cx,
                Event::MessageAppended {
                    turn: turn.into(),
                    message: message.clone(),
                    usage: Some(resp.usage),
                },
            )
            .await;
        shared
            .hooks
            .run(
                hook::StepEnd {
                    usage: resp.usage,
                    context_tokens: live.estimate(prompt),
                    context_window: window,
                },
                &cx,
            )
            .await;

        let calls = message.tool_calls();
        let finished = if calls.is_empty() {
            true
        } else {
            let cut_off = resp.stop == StopReason::Length;
            let batch = run_tools(shared, live, &cx, turn, &calls, &s.tools, cut_off).await;
            commit(shared, live, &cx, turn, batch.results, batch.writes).await?;
            batch.terminate
        };
        if !finished {
            let steered = std::mem::take(&mut shared.queue.lock().unwrap().steer);
            push_user(shared, live, steered).await?;
            continue;
        }

        let tools = cx.tools.clone();
        let mut cx = shared.cx(live, Some(turn), Some(step), abort);
        cx.tools = tools;
        let next = shared
            .hooks
            .run(
                hook::TurnEnd {
                    message: message.clone(),
                    usage: *usage,
                },
                &cx,
            )
            .await;
        let mut more = match next {
            Next::FollowUp(t) => vec![t],
            Next::Done => Vec::new(),
        };
        {
            let mut queue = shared.queue.lock().unwrap();
            more.append(&mut queue.steer);
            more.append(&mut queue.follow);
            // Stop accepting messages in the same breath as finding none: one that arrives
            // after this is refused, so its sender can start a new turn instead.
            if more.is_empty() {
                queue.running = false;
            }
        }
        if more.is_empty() {
            flush(shared, live, Some(turn), abort).await?;
            return Ok(Some(message));
        }
        push_user(shared, live, more).await?;
    }
}

/// Streams one model call, forwarding deltas. `None` means the turn was aborted before the
/// provider returned a response.
async fn stream(
    shared: &Shared,
    cx: &HookCx,
    turn: &str,
    req: ModelRequest,
    abort: &Abort,
) -> Option<Response> {
    let mut stream = shared.rt.provider.stream(req, abort.clone());
    loop {
        let ev = tokio::select! {
            ev = stream.next() => ev,
            _ = abort.aborted() => return None,
        };
        let event = match ev {
            None => {
                return Some(Response::error(
                    crate::types::ErrorKind::Other,
                    "provider stream ended without a final response",
                ));
            }
            Some(StreamEvent::Done(r)) => return Some(r),
            Some(StreamEvent::TextDelta(text)) => Event::TextDelta {
                turn: turn.into(),
                text,
            },
            Some(StreamEvent::ReasoningDelta(text)) => Event::ReasoningDelta {
                turn: turn.into(),
                text,
            },
            Some(StreamEvent::ToolCallStarted { .. }) => continue,
        };
        shared.emit(cx, event).await;
    }
}

type StateWrites = Vec<(String, Value)>;

struct Batch {
    /// One result per call, in call order.
    results: Vec<Content>,
    writes: StateWrites,
    terminate: bool,
}

enum Plan {
    Done(ToolCall, ToolResult),
    Run(ToolCall, Tool),
}

/// Runs the `ToolCall` hooks in call order, then the calls still to run (in parallel, except
/// sequential tools), each followed by its `ToolResult` hooks.
async fn run_tools(
    shared: &Arc<Shared>,
    live: &Live,
    cx: &HookCx,
    turn: &str,
    calls: &[ToolCall],
    tools: &[Tool],
    cut_off: bool,
) -> Batch {
    let mut plans = Vec::with_capacity(calls.len());
    for call in calls {
        let plan = if cut_off {
            Plan::Done(call.clone(), ToolResult::error(CUT_OFF))
        } else {
            let e = hook::ToolCall {
                call: call.clone(),
                tool: tools.iter().find(|t| t.name() == call.name).cloned(),
            };
            match shared.hooks.run(e, cx).await {
                Decision::Run(e) => {
                    let mut c = e.call;
                    c.id = call.id.clone();
                    resolve(c, tools)
                }
                Decision::Deny(reason) => {
                    Plan::Done(call.clone(), ToolResult::error(format!("denied: {reason}")))
                }
                Decision::Result(r) => Plan::Done(call.clone(), r),
            }
        };
        plans.push(plan);
    }

    let mut done: Vec<Option<(ToolResult, StateWrites)>> = (0..calls.len()).map(|_| None).collect();
    let mut group = Vec::new();
    for (i, p) in plans.iter().enumerate() {
        match p {
            Plan::Done(call, r) => {
                let r = finish(shared, cx, turn, call, r.clone()).await;
                done[i] = Some((r, Vec::new()));
            }
            Plan::Run(call, tool) => {
                let fut = run_one(shared, live, cx, turn, i, call, tool);
                if tool.is_sequential() {
                    for (j, r, w) in futures_util::future::join_all(group.drain(..)).await {
                        done[j] = Some((r, w));
                    }
                    let (j, r, w) = fut.await;
                    done[j] = Some((r, w));
                } else {
                    group.push(fut);
                }
            }
        }
    }
    for (j, r, w) in futures_util::future::join_all(group).await {
        done[j] = Some((r, w));
    }

    let mut batch = Batch {
        results: Vec::with_capacity(calls.len()),
        writes: Vec::new(),
        terminate: false,
    };
    for (call, d) in calls.iter().zip(done) {
        let (r, w) = d.unwrap_or_else(|| (ToolResult::error("not run"), Vec::new()));
        batch.terminate |= r.terminate;
        batch.writes.extend(w);
        batch.results.push(Content::ToolResult {
            call_id: call.id.clone(),
            output: r.output,
            is_error: r.is_error,
        });
    }
    batch
}

/// The tool a call names (deferred tools included), and a JSON object input.
fn resolve(call: ToolCall, tools: &[Tool]) -> Plan {
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        let mut known: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        known.truncate(30);
        let msg = format!(
            "unknown tool `{}`; the tools are: {}",
            call.name,
            known.join(", ")
        );
        return Plan::Done(call, ToolResult::error(msg));
    };
    if !call.input.is_object() {
        let msg = format!(
            "the arguments were not a JSON object; `{}` takes {}",
            call.name,
            tool.spec().input_schema
        );
        return Plan::Done(call, ToolResult::error(msg));
    }
    Plan::Run(call, tool.clone())
}

async fn run_one(
    shared: &Arc<Shared>,
    live: &Live,
    cx: &HookCx,
    turn: &str,
    i: usize,
    call: &ToolCall,
    tool: &Tool,
) -> (usize, ToolResult, StateWrites) {
    let writes: Writes = Arc::default();
    let tcx = ToolCx {
        call_id: call.id.clone(),
        session: shared.id.clone(),
        abort: cx.abort.clone(),
        state: live.state.clone(),
        writes: writes.clone(),
        env: shared.env().clone(),
        runtime: Runtime::from_inner(shared.rt.clone()),
        log: {
            let events = shared.events.clone();
            let (turn, call_id) = (turn.to_string(), call.id.clone());
            Arc::new(move |text| {
                let _ = events.send(Event::ActionLog {
                    turn: turn.clone(),
                    call_id: call_id.clone(),
                    text,
                });
            })
        },
    };
    shared
        .emit(
            cx,
            Event::ActionStarted {
                turn: turn.into(),
                call_id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            },
        )
        .await;
    let r = AssertUnwindSafe(tool.call(call.input.clone(), tcx))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| ToolResult::error("the tool panicked"));
    let r = finish(shared, cx, turn, call, r).await;
    let w = std::mem::take(&mut *writes.lock().unwrap());
    (i, r, w)
}

/// Runs the `ToolResult` hooks for one call and reports the result.
async fn finish(
    shared: &Shared,
    cx: &HookCx,
    turn: &str,
    call: &ToolCall,
    result: ToolResult,
) -> ToolResult {
    let e = hook::ToolResult {
        call: call.clone(),
        result,
    };
    let r = shared.hooks.run(e, cx).await.result;
    shared
        .emit(
            cx,
            Event::ActionResult {
                turn: turn.into(),
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: r.output.clone(),
                is_error: r.is_error,
            },
        )
        .await;
    r
}

/// Records a finished tool batch with its state writes.
async fn commit(
    shared: &Shared,
    live: &mut Live,
    cx: &HookCx,
    turn: &str,
    results: Vec<Content>,
    writes: Vec<(String, Value)>,
) -> Result<(), StoreError> {
    let message = Message::new(Role::Tool, results);
    let mut records = vec![Record::Message {
        message: message.clone(),
    }];
    records.extend(
        writes
            .into_iter()
            .map(|(key, value)| Record::State { key, value }),
    );
    append(shared, live, records).await?;
    shared
        .emit(
            cx,
            Event::MessageAppended {
                turn: turn.into(),
                message,
                usage: None,
            },
        )
        .await;
    Ok(())
}

async fn push_user(shared: &Shared, live: &mut Live, texts: Vec<String>) -> Result<(), StoreError> {
    let records = texts
        .into_iter()
        .map(|t| Record::Message {
            message: Message::user(t),
        })
        .collect();
    append(shared, live, records).await
}

/// Appends records in order, chaining tree records onto the current leaf.
pub(crate) async fn append(
    shared: &Shared,
    live: &mut Live,
    records: Vec<Record>,
) -> Result<(), StoreError> {
    if records.is_empty() {
        return Ok(());
    }
    let mut entries = Vec::with_capacity(records.len());
    for r in records {
        let parent = if r.is_meta() { None } else { live.leaf.clone() };
        let e = Entry::new(parent, r);
        live.apply(&e);
        entries.push(e);
    }
    shared.rt.store.append(&shared.id, &entries).await?;
    shared.publish_status(live);
    Ok(())
}

/// Applies what hooks asked for (state, custom records, notes) and runs a compaction one asked
/// for. Called only where no tool batch is open. Compactions asked for while compacting are
/// dropped.
pub(crate) async fn flush(
    shared: &Arc<Shared>,
    live: &mut Live,
    turn: Option<&str>,
    abort: &Abort,
) -> Result<(), Error> {
    for pass in 0..2 {
        let ops = std::mem::take(&mut *shared.ops.lock().unwrap());
        let mut records: Vec<Record> = ops
            .writes
            .into_iter()
            .map(|(key, value)| Record::State { key, value })
            .collect();
        records.extend(
            ops.custom
                .into_iter()
                .map(|(kind, data)| Record::Custom { kind, data }),
        );
        records.extend(ops.notes.into_iter().map(|t| Record::Message {
            message: Message::system_note(t),
        }));
        append(shared, live, records).await?;
        if let Some(reason) = ops.fail
            && turn.is_some()
        {
            return Err(Error::Failed(reason));
        }
        match ops.compact {
            Some((compactor, focus)) if pass == 0 => {
                compact(shared, live, compactor, focus, turn, abort).await?;
            }
            _ => break,
        }
    }
    Ok(())
}

/// Summarises the context before the recent window: plans the cut, runs `BeforeCompact`, asks
/// the compactor (unless a hook gave a summary), records the compaction and runs `Compacted`.
pub(crate) async fn compact(
    shared: &Arc<Shared>,
    live: &mut Live,
    compactor: Arc<dyn Compactor>,
    focus: Option<String>,
    turn: Option<&str>,
    abort: &Abort,
) -> Result<CompactReport, Error> {
    let Some(model) = shared.model(live) else {
        return Err(Error::Failed("no model configured".into()));
    };
    let system = shared.agent.instructions.clone();
    let prompt = prompt_tokens(&system, &[]);
    let tokens_before = live.estimate(prompt);
    let mut report = CompactReport {
        summarized: false,
        tokens_before,
        tokens_after: tokens_before,
    };
    let keep = compactor
        .keep_recent_tokens()
        .min(live.messages_tokens() / 4);
    let Some(cut) = live.cut_point(keep) else {
        return Ok(report);
    };
    let plan = CompactPlan {
        messages: live.messages[..cut].to_vec(),
        previous_summary: live.summary.clone(),
        first_kept: live.ids[cut].clone(),
        tokens_before,
        focus,
    };
    let cx = shared.cx(live, turn, None, abort);
    let first_kept = plan.first_kept.clone();
    let (first_kept, summary) = match shared.hooks.run(hook::BeforeCompact { plan }, &cx).await {
        CompactDecision::Cancel => return Ok(report),
        CompactDecision::Summary(s) => (first_kept, s),
        CompactDecision::Proceed(e) => {
            let plan = e.plan;
            if !live
                .ids
                .iter()
                .any(|id| !id.is_empty() && *id == plan.first_kept)
            {
                return Err(Error::Failed(format!(
                    "compaction: no message {} in the context",
                    plan.first_kept
                )));
            }
            let ccx = CompactCx {
                provider: shared.rt.provider.clone(),
                model,
                abort: abort.clone(),
            };
            let summary = compactor
                .summarize(&plan, &ccx)
                .await
                .map_err(|e| Error::Failed(format!("compaction failed: {e}")))?;
            (plan.first_kept, summary)
        }
    };
    append(
        shared,
        live,
        vec![Record::Compaction {
            summary: summary.clone(),
            first_kept: first_kept.clone(),
            tokens_before,
            details: None,
        }],
    )
    .await?;
    report.summarized = true;
    report.tokens_after = live.estimate(prompt);
    let cx = shared.cx(live, turn, None, abort);
    shared
        .hooks
        .run(
            hook::Compacted {
                summary,
                first_kept,
                tokens_before,
                tokens_after: report.tokens_after,
            },
            &cx,
        )
        .await;
    shared
        .emit(
            &cx,
            Event::Compacted {
                turn: turn.unwrap_or_default().into(),
                tokens_before,
                tokens_after: report.tokens_after,
            },
        )
        .await;
    Ok(report)
}
