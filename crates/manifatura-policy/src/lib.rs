//! Approval policy for manifatura: rules that allow, ask about or deny tool calls, by tool and,
//! for shell commands, by command prefix. A command line is split into the simple commands it
//! runs ([`shell::parse`], on tree-sitter-bash), and each is checked.
//!
//! ```no_run
//! use manifatura_kit::Approvals;
//! use manifatura_policy::{Decision, Policy, Rules};
//!
//! let rules: Rules = serde_json::from_str(r#"{
//!     "allow": ["exec_command(git commit)", "mcp__exa__*"],
//!     "deny": ["exec_command(git push --force)"]
//! }"#).unwrap();
//! let approvals = Approvals::new();
//! let policy = Policy::new()
//!     .rules(&rules).unwrap()
//!     .tool_default("read", Decision::Allow)
//!     .default(Decision::Ask);
//! let agent = manifatura_agent::Agent::new("…").hooks(policy.hooks(&approvals));
//! ```
//!
//! How a call is decided:
//!
//! 1. Rules: a `deny` rule that matches the call (or, for a command, any simple command in
//!    it) denies it; otherwise an `ask` rule asks. An `allow` rule allows a call of a tool it
//!    names; for a command, every simple command must be allowed by a rule or only read
//!    ([`shell::read_only`]), and the line must not be opaque.
//! 2. Otherwise the defaults, in the order given: [`Policy::when`] and
//!    [`Policy::tool_default`]; then [`Policy::default`] (ask, unless set).
//!
//! Asking goes through [`Approvals`]: the request offers, for a command, the rules that would
//! allow it next time (`exec_command(git commit)`, never `rm` or `bash`), which
//! [`Answer::AllowAndRemember`] adds to the policy and hands to [`Policy::on_remember`] for the
//! app to save. [`Answer::AllowAlways`] allows the same call (a command) or the tool (anything
//! else) for the rest of the session. Rules are not a security boundary: a sandbox is.

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod shell;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use manifatura_agent::{Hooks, SessionId, ToolCall, hook};
use manifatura_kit::{Answer, Approvals, Request, approval_key, call_scope, denial, record};
use serde::{Deserialize, Serialize};

/// What to do with a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Run it.
    Allow,
    /// Ask someone first.
    Ask,
    /// Refuse it.
    Deny,
}

/// One rule: a tool name (`*` matches any run of characters, as in `mcp__github__*`) and,
/// optionally, a command prefix for tools that run shell commands. Written
/// `exec_command(git commit)`; a trailing `*` or `:*` in the prefix is ignored.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Rule {
    /// The tool, or a pattern of tool names.
    pub tool: String,
    /// For a command tool: the words a simple command starts with.
    pub command: Option<Vec<String>>,
}

impl Rule {
    /// A rule for every call of the tools `tool` names.
    pub fn tool(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            command: None,
        }
    }

    /// A rule for commands of the tool `tool` that start with `prefix`.
    pub fn command<S: Into<String>>(
        tool: impl Into<String>,
        prefix: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            tool: tool.into(),
            command: Some(prefix.into_iter().map(Into::into).collect()),
        }
    }

    /// Reads `tool` or `tool(prefix words)`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let Some(open) = text.find('(') else {
            if text.is_empty() || text.contains(char::is_whitespace) {
                return Err(format!(
                    "`{text}` is not a rule: write `tool` or `tool(command prefix)`"
                ));
            }
            return Ok(Rule::tool(text));
        };
        let (tool, rest) = text.split_at(open);
        let Some(inner) = rest[1..].strip_suffix(')') else {
            return Err(format!("`{text}`: the command prefix must end with `)`"));
        };
        let mut words: Vec<String> = inner.split_whitespace().map(str::to_string).collect();
        if let Some(last) = words.last_mut() {
            if last == "*" {
                words.pop();
            } else if let Some(s) = last.strip_suffix(":*") {
                *last = s.to_string();
            }
        }
        if tool.trim().is_empty() || words.is_empty() {
            return Err(format!("`{text}` names no tool or no command"));
        }
        Ok(Rule::command(tool.trim(), words))
    }

    fn matches_tool(&self, name: &str) -> bool {
        glob(&self.tool, name)
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.command {
            Some(words) => write!(f, "{}({})", self.tool, words.join(" ")),
            None => f.write_str(&self.tool),
        }
    }
}

impl std::str::FromStr for Rule {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Rule::parse(s)
    }
}

/// `*` matches any run of characters; everything else matches itself.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name[first.len()..].ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

/// Rules as a config file holds them: three lists of rule texts.
///
/// ```toml
/// allow = ["exec_command(git commit)", "mcp__exa__*"]
/// ask = ["mcp__github__*"]
/// deny = ["exec_command(git push --force)"]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Rules that allow.
    pub allow: Vec<String>,
    /// Rules that ask.
    pub ask: Vec<String>,
    /// Rules that deny.
    pub deny: Vec<String>,
}

/// What [`Policy::decide`] concluded.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Verdict {
    /// The decision.
    pub decision: Decision,
    /// The rule that decided, if one did.
    pub rule: Option<Rule>,
    /// For a call that asks: the rules that would allow it next time, if it is safe to offer
    /// them.
    pub remember: Vec<Rule>,
}

type When = Arc<dyn Fn(&ToolCall, &SessionId) -> bool + Send + Sync>;
type Remember = Arc<dyn Fn(&Rule) + Send + Sync>;

/// Rules and defaults deciding which calls run, which ask and which are refused. Cheap to
/// clone; clones share their rules, so a rule remembered in one session holds in all.
#[derive(Clone)]
pub struct Policy(Arc<RwLock<Inner>>);

struct Inner {
    rules: Vec<(Rule, Decision)>,
    defaults: Vec<(When, Decision)>,
    default: Decision,
    /// Tools that run shell commands, by the input field holding the command.
    command_fields: HashMap<String, String>,
    read_only_allowed: bool,
    on_remember: Option<Remember>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.read().unwrap();
        f.debug_struct("Policy")
            .field("rules", &inner.rules)
            .field("defaults", &inner.defaults.len())
            .field("default", &inner.default)
            .field("command_fields", &inner.command_fields)
            .finish_non_exhaustive()
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    /// No rules; every call asks. `bash` (`command`) and `exec_command` (`cmd`) are known as
    /// command tools, and read-only commands are allowed.
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(Inner {
            rules: Vec::new(),
            defaults: Vec::new(),
            default: Decision::Ask,
            command_fields: [("bash", "command"), ("exec_command", "cmd")]
                .into_iter()
                .map(|(t, f)| (t.to_string(), f.to_string()))
                .collect(),
            read_only_allowed: true,
            on_remember: None,
        })))
    }

    fn edit(self, f: impl FnOnce(&mut Inner)) -> Self {
        f(&mut self.0.write().unwrap());
        self
    }

    /// Adds a rule.
    pub fn rule(self, rule: Rule, decision: Decision) -> Self {
        self.edit(|i| i.rules.push((rule, decision)))
    }

    /// Adds the rules of a config file.
    pub fn rules(self, rules: &Rules) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for (list, decision) in [
            (&rules.allow, Decision::Allow),
            (&rules.ask, Decision::Ask),
            (&rules.deny, Decision::Deny),
        ] {
            for text in list {
                parsed.push((Rule::parse(text)?, decision));
            }
        }
        Ok(self.edit(|i| i.rules.extend(parsed)))
    }

    /// Decides calls no rule decides for which `when` is true; checked in the order added,
    /// with [`tool_default`](Self::tool_default), before the [`default`](Self::default).
    pub fn when(
        self,
        when: impl Fn(&ToolCall, &SessionId) -> bool + Send + Sync + 'static,
        decision: Decision,
    ) -> Self {
        self.edit(|i| i.defaults.push((Arc::new(when), decision)))
    }

    /// Decides calls of `tool` (a name or pattern) that no rule decides.
    pub fn tool_default(self, tool: impl Into<String>, decision: Decision) -> Self {
        let tool = tool.into();
        self.when(move |call, _| glob(&tool, &call.name), decision)
    }

    /// Decides what nothing else decides (default: ask).
    pub fn default(self, decision: Decision) -> Self {
        self.edit(|i| i.default = decision)
    }

    /// Makes `tool` a command tool whose input field `field` holds a shell command line.
    pub fn command_tool(self, tool: impl Into<String>, field: impl Into<String>) -> Self {
        self.edit(|i| {
            i.command_fields.insert(tool.into(), field.into());
        })
    }

    /// Whether commands that only read are allowed without a rule (default: yes).
    pub fn allow_read_only(self, on: bool) -> Self {
        self.edit(|i| i.read_only_allowed = on)
    }

    /// Called with each rule an [`Answer::AllowAndRemember`] adds, for the app to save.
    pub fn on_remember(self, f: impl Fn(&Rule) + Send + Sync + 'static) -> Self {
        self.edit(|i| i.on_remember = Some(Arc::new(f)))
    }

    /// Adds a rule to a policy in use.
    pub fn add_rule(&self, rule: Rule, decision: Decision) {
        self.0.write().unwrap().rules.push((rule, decision));
    }

    /// The rules, in the order added.
    pub fn list(&self) -> Vec<(Rule, Decision)> {
        self.0.read().unwrap().rules.clone()
    }

    /// The command line of `call`, for a command tool.
    fn command_line(&self, call: &ToolCall) -> Option<String> {
        let inner = self.0.read().unwrap();
        let field = inner.command_fields.get(&call.name)?;
        call.input[field.as_str()].as_str().map(str::to_string)
    }

    /// Decides `call`, made in `session`.
    pub fn decide(&self, call: &ToolCall, session: &SessionId) -> Verdict {
        let line = self.command_line(call);
        let inner = self.0.read().unwrap();
        let rules: Vec<&(Rule, Decision)> = inner
            .rules
            .iter()
            .filter(|(r, _)| r.matches_tool(&call.name))
            .collect();
        let tool_rule = |d: Decision| {
            rules
                .iter()
                .find(|(r, dec)| *dec == d && r.command.is_none())
                .map(|(r, _)| r.clone())
        };
        let verdict = |decision, rule| Verdict {
            decision,
            rule,
            remember: Vec::new(),
        };

        let parsed = line.as_deref().map(shell::parse);
        // The rule of `decision` matching any of the simple commands.
        let command_rule = |d: Decision| -> Option<Rule> {
            let parsed = parsed.as_ref()?;
            rules.iter().find_map(|(r, dec)| {
                let prefix = r.command.as_ref()?;
                (*dec == d && parsed.commands.iter().any(|c| c.words.starts_with(prefix)))
                    .then(|| r.clone())
            })
        };
        for d in [Decision::Deny, Decision::Ask] {
            if let Some(r) = tool_rule(d).or_else(|| command_rule(d)) {
                return verdict(d, Some(r));
            }
        }
        if let Some(r) = tool_rule(Decision::Allow) {
            return verdict(Decision::Allow, Some(r));
        }
        let mut remember = Vec::new();
        if let Some(parsed) = &parsed {
            let mut unallowed = Vec::new();
            let mut deciding = None;
            for c in &parsed.commands {
                let by_rule = rules.iter().find(|(r, d)| {
                    *d == Decision::Allow
                        && r.command.as_ref().is_some_and(|p| c.words.starts_with(p))
                });
                match by_rule {
                    Some((r, _)) => deciding = Some(r.clone()),
                    None if inner.read_only_allowed && shell::read_only(c) => {}
                    None => unallowed.push(c),
                }
            }
            if !parsed.opaque && unallowed.is_empty() && !parsed.commands.is_empty() {
                return verdict(Decision::Allow, deciding);
            }
            if !parsed.opaque {
                for c in unallowed {
                    match shell::suggest(c) {
                        Some(prefix) => {
                            let r = Rule::command(call.name.clone(), prefix);
                            if !remember.contains(&r) {
                                remember.push(r);
                            }
                        }
                        // Offer rules only when they would allow the whole line next time.
                        None => {
                            remember.clear();
                            break;
                        }
                    }
                }
            }
        }
        let decision = inner
            .defaults
            .iter()
            .find(|(when, _)| when(call, session))
            .map_or(inner.default, |(_, d)| *d);
        Verdict {
            decision,
            rule: None,
            remember: if decision == Decision::Ask {
                remember
            } else {
                Vec::new()
            },
        }
    }

    /// A `ToolCall` hook applying the policy: allowed calls run, denied ones are refused with
    /// the rule that refused them, and the rest ask through `approvals`. The request's reason
    /// is the rule that asks, or the call's `justification` field when it has one (as an
    /// `exec_command` asking to leave the sandbox does). Answers are recorded in the session
    /// (custom `approval` records).
    pub fn hooks(&self, approvals: &Approvals) -> Hooks {
        let (policy, approvals) = (self.clone(), approvals.clone());
        Hooks::new().on(move |e: hook::ToolCall, cx| {
            let (policy, approvals) = (policy.clone(), approvals.clone());
            async move {
                let v = policy.decide(&e.call, cx.session_id());
                match v.decision {
                    Decision::Allow => return hook::Decision::Run(e),
                    Decision::Deny => {
                        return hook::Decision::Deny(match &v.rule {
                            Some(r) => format!("the policy denies this call (rule `{r}`)"),
                            None => "the policy denies this call".into(),
                        });
                    }
                    Decision::Ask => {}
                }
                // "Always" covers the same command line, or the whole tool.
                let scope = if policy.command_line(&e.call).is_some() {
                    call_scope(&e.call)
                } else {
                    e.call.name.clone()
                };
                let key = approval_key(&scope);
                if cx.state::<bool>(&key) == Some(true) {
                    return hook::Decision::Run(e);
                }
                let mut request =
                    Request::new(cx.session_id().clone(), e.call.clone()).scope(scope);
                if let Some(j) = e.call.input["justification"].as_str() {
                    request = request.reason(j);
                } else if let Some(r) = &v.rule {
                    request = request.reason(format!("rule `{r}` asks"));
                }
                if !v.remember.is_empty() {
                    let texts: Vec<String> = v.remember.iter().map(Rule::to_string).collect();
                    request = request.remember(texts.join(", "));
                }
                let answer = approvals.ask(request, cx.abort()).await;
                record(&cx, &e.call, &answer);
                match answer {
                    Answer::Deny(reason) => hook::Decision::Deny(denial(reason.as_deref())),
                    Answer::AllowAndRemember if !v.remember.is_empty() => {
                        let save = policy.0.read().unwrap().on_remember.clone();
                        for r in v.remember {
                            policy.add_rule(r.clone(), Decision::Allow);
                            if let Some(save) = &save {
                                save(&r);
                            }
                        }
                        hook::Decision::Run(e)
                    }
                    Answer::AllowAlways | Answer::AllowAndRemember => {
                        cx.set_state(key, true);
                        hook::Decision::Run(e)
                    }
                    _ => hook::Decision::Run(e),
                }
            }
        })
    }
}
