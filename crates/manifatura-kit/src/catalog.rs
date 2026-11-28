//! The deferred catalog (eve): tools flagged `deferred` stay out of the prompt (the core never
//! offers them). The model finds them with `find_tools` (BM25 over name, description and input
//! fields) and runs them with `run_tool`, which a `ToolCall` hook unwraps into a call of the
//! real tool, so later hooks, approvals and events see the real name.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use manifatura_agent::{Hooks, Tool, ToolCall, ToolCx, ToolHandler, ToolResult, ToolSpec, hook};
use serde_json::{Value, json};

/// The name of the tool the model searches the catalog with.
pub const SEARCH_TOOL: &str = "find_tools";
/// The name of the tool the model runs a catalog tool through.
pub const CALL_TOOL: &str = "run_tool";

const RESULTS: usize = 5;

/// Offers `find_tools` and `run_tool` whenever a step has deferred tools, and turns `run_tool`
/// into a call of the tool it names. Adding it twice does no harm.
pub fn catalog() -> Hooks {
    // `find_tools` and `run_tool` for the last set of deferred tools, by name: steps usually
    // defer the same tools, and the search index is worth building once.
    let built: Arc<Mutex<(Vec<String>, Vec<Tool>)>> = Arc::default();
    Hooks::new()
        .on(move |e: hook::Step, _cx| {
            let built = built.clone();
            async move {
                let mut e = e.without_tool(SEARCH_TOOL).without_tool(CALL_TOOL);
                let deferred: Vec<&Tool> = e.tools.iter().filter(|t| t.is_deferred()).collect();
                if deferred.is_empty() {
                    return e;
                }
                let names: Vec<String> = deferred.iter().map(|t| t.name().to_owned()).collect();
                let mut built = built.lock().unwrap();
                if built.0 != names {
                    *built = (names, tools(&deferred));
                }
                let catalog = built.1.clone();
                drop(built);
                e.tools.extend(catalog);
                e
            }
        })
        .on(|mut e: hook::ToolCall, cx| async move {
            if e.call.name != CALL_TOOL {
                return hook::Decision::Run(e);
            }
            match unwrap_call(&e.call) {
                Ok(call) => {
                    e.tool = cx.tool(&call.name).cloned();
                    e.call = call;
                    hook::Decision::Run(e)
                }
                Err(msg) => hook::Decision::Result(ToolResult::error(msg)),
            }
        })
}

/// `find_tools` and `run_tool` for these deferred tools.
fn tools(deferred: &[&Tool]) -> Vec<Tool> {
    let specs: Vec<ToolSpec> = deferred.iter().map(|t| t.spec().clone()).collect();
    let mut namespaces: BTreeMap<&str, usize> = BTreeMap::new();
    for s in &specs {
        *namespaces.entry(namespace(&s.name)).or_default() += 1;
    }
    let listing: Vec<String> = namespaces
        .iter()
        .map(|(ns, n)| format!("{ns} ({n} tool{})", if *n == 1 { "" } else { "s" }))
        .collect();
    let search = Tool::from_handler(
        ToolSpec::new(
            SEARCH_TOOL,
            format!(
                "Search the catalog of additional tools by keywords. Returns matching tool names, \
                 descriptions and input schemas; run one with {CALL_TOOL}. Catalog: {}.",
                listing.join(", ")
            ),
            json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Keywords." } },
                "required": ["query"]
            }),
        ),
        Search {
            index: Index::new(&specs),
            specs,
        },
    );
    let call = Tool::from_handler(
        ToolSpec::new(
            CALL_TOOL,
            format!("Run a tool found with {SEARCH_TOOL}."),
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The tool's name." },
                    "input": { "type": "object", "description": "The tool's input." }
                },
                "required": ["name", "input"]
            }),
        ),
        Unwrapped,
    );
    vec![search, call]
}

/// The call `run_tool` stands for, keeping the original call id.
pub fn unwrap_call(call: &ToolCall) -> Result<ToolCall, String> {
    let name = call.input["name"]
        .as_str()
        .ok_or_else(|| format!("{CALL_TOOL} needs a `name`"))?;
    if name == CALL_TOOL || name == SEARCH_TOOL {
        return Err(format!("{CALL_TOOL} cannot run `{name}`"));
    }
    let input = match &call.input["input"] {
        // Small models often put the arguments next to `name` instead of inside `input`.
        Value::Null => match &call.input {
            Value::Object(o) => Value::Object(
                o.iter()
                    .filter(|(k, _)| k.as_str() != "name")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            _ => json!({}),
        },
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
        v => v.clone(),
    };
    Ok(ToolCall {
        id: call.id.clone(),
        name: name.to_string(),
        input,
    })
}

/// The part before `__` (MCP style) or `.`; tools without one share `general`.
fn namespace(name: &str) -> &str {
    name.split_once("__")
        .or_else(|| name.split_once('.'))
        .map(|(ns, _)| ns)
        .unwrap_or("general")
}

struct Unwrapped;

#[async_trait]
impl ToolHandler for Unwrapped {
    async fn call(&self, _input: Value, _cx: ToolCx) -> ToolResult {
        ToolResult::error(format!(
            "{CALL_TOOL} is handled by the catalog's ToolCall hook, which did not run"
        ))
    }
}

struct Search {
    specs: Vec<ToolSpec>,
    index: Index,
}

#[async_trait]
impl ToolHandler for Search {
    async fn call(&self, input: Value, _cx: ToolCx) -> ToolResult {
        let query = input["query"].as_str().unwrap_or_default();
        let hits = self.index.search(query, RESULTS);
        if hits.is_empty() {
            let names: Vec<&str> = self.specs.iter().map(|s| s.name.as_str()).collect();
            return ToolResult::ok(format!(
                "No tools matched `{query}`. All tools: {}",
                names.join(", ")
            ));
        }
        let mut out = String::new();
        for i in hits {
            let s = &self.specs[i];
            out.push_str(&format!(
                "{} — {}\n  input: {}\n",
                s.name, s.description, s.input_schema
            ));
        }
        ToolResult::ok(out.trim_end().to_string())
    }
}

/// BM25 over tool documents.
struct Index {
    docs: Vec<HashMap<String, f64>>,
    lengths: Vec<f64>,
    df: HashMap<String, f64>,
    avg: f64,
}

impl Index {
    fn new(specs: &[ToolSpec]) -> Self {
        let mut docs = Vec::with_capacity(specs.len());
        let mut lengths = Vec::with_capacity(specs.len());
        let mut df: HashMap<String, f64> = HashMap::new();
        for s in specs {
            let mut tf: HashMap<String, f64> = HashMap::new();
            let mut n = 0.0;
            // The name counts double: it is the strongest signal.
            let name = words(&s.name);
            let fields = schema_words(&s.input_schema);
            for w in name
                .iter()
                .chain(name.iter())
                .chain(words(&s.description).iter())
                .chain(fields.iter())
            {
                *tf.entry(w.clone()).or_default() += 1.0;
                n += 1.0;
            }
            for w in tf.keys() {
                *df.entry(w.clone()).or_default() += 1.0;
            }
            docs.push(tf);
            lengths.push(n);
        }
        let avg = lengths.iter().sum::<f64>() / lengths.len().max(1) as f64;
        Self {
            docs,
            lengths,
            df,
            avg,
        }
    }

    /// Indices of the best matches, best first.
    fn search(&self, query: &str, limit: usize) -> Vec<usize> {
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let n = self.docs.len() as f64;
        let terms = words(query);
        let mut scored: Vec<(usize, f64)> = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, tf)| {
                let score = terms
                    .iter()
                    .filter_map(|t| {
                        let f = *tf.get(t)?;
                        let df = self.df.get(t).copied().unwrap_or(0.0);
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        let norm = 1.0 - B + B * self.lengths[i] / self.avg.max(1.0);
                        Some(idf * f * (K1 + 1.0) / (f + K1 * norm))
                    })
                    .sum::<f64>();
                (i, score)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().take(limit).map(|(i, _)| i).collect()
    }
}

/// Lowercase words, split on anything that is not a letter or digit, with a crude plural fold.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(|w| {
            let w = w.to_lowercase();
            match w.strip_suffix('s') {
                Some(stem) if stem.len() > 2 && !stem.ends_with('s') => stem.to_string(),
                _ => w,
            }
        })
        .collect()
}

fn schema_words(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(props) = schema["properties"].as_object() {
        for (k, v) in props {
            out.extend(words(k));
            if let Some(d) = v["description"].as_str() {
                out.extend(words(d));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            description,
            json!({"type": "object", "properties": {"repo": {"type": "string"}}}),
        )
    }

    #[test]
    fn bm25_ranks_names_and_descriptions() {
        let specs = [
            spec("github__create_issue", "Open a new issue in a repository."),
            spec("github__list_pulls", "List pull requests."),
            spec("calendar__add_event", "Add an event to the calendar."),
        ];
        let index = Index::new(&specs);
        assert_eq!(index.search("create issue", 5)[0], 0);
        assert_eq!(index.search("pull requests", 5)[0], 1);
        assert_eq!(index.search("calendar events", 5), [2]);
        assert!(index.search("weather", 5).is_empty());
        assert_eq!(namespace("github__list_pulls"), "github");
        assert_eq!(namespace("lookup"), "general");
    }

    #[test]
    fn unwrap_keeps_the_call_id() {
        let c = ToolCall {
            id: "c1".into(),
            name: CALL_TOOL.into(),
            input: json!({"name": "x", "input": "{\"a\":1}"}),
        };
        let u = unwrap_call(&c).unwrap();
        assert_eq!((u.id.as_str(), u.name.as_str()), ("c1", "x"));
        assert_eq!(u.input, json!({"a": 1}));
    }
}
