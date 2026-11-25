//! Input repair for models that get JSON types wrong.

use manifatura_agent::{Hooks, ToolSpec, hook};
use serde_json::{Value, json};

/// Coerces each call's input against its tool's schema before later hooks see it: `"30"` for
/// an integer, `"true"` for a boolean, objects and arrays sent as JSON strings, numbers for
/// strings, and a bare value for a tool with one field.
pub fn repair() -> Hooks {
    Hooks::new().on(|mut e: hook::ToolCall, cx| async move {
        let tool = e.tool.clone().or_else(|| cx.tool(&e.call.name).cloned());
        if let Some(tool) = tool {
            e.call.input = repair_input(std::mem::take(&mut e.call.input), tool.spec());
        }
        hook::Decision::Run(e)
    })
}

/// One input, repaired against `spec`'s schema.
pub fn repair_input(input: Value, spec: &ToolSpec) -> Value {
    let schema = &spec.input_schema;
    let input = coerce_input(input, schema);
    if input.is_object() {
        return input;
    }
    match sole_field(schema) {
        // A bare value for a tool with one field: `"Lisbon"` for `{"city": …}`.
        Some(field) => coerce_input(json!({ field: input }), schema),
        None => input,
    }
}

/// The only field of an object schema, or its only required one.
fn sole_field(schema: &Value) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    if props.len() == 1 {
        return props.keys().next().cloned();
    }
    match schema.get("required")?.as_array()?.as_slice() {
        [Value::String(only)] => Some(only.clone()),
        _ => None,
    }
}

/// Repairs common mistakes of small models against a tool's input schema before the tool sees
/// the input: `"30"` where a number is expected, `"true"` for a boolean, an object or array
/// sent as a JSON string, and a number where a string is expected. Anything else is left as
/// it is, for the tool to reject.
fn coerce_input(v: Value, schema: &Value) -> Value {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let has = |t: &str| types.contains(&t);
    match v {
        Value::String(s) if !types.is_empty() && !has("string") => {
            let t = s.trim();
            if has("integer") {
                if let Ok(n) = t.parse::<i64>() {
                    return n.into();
                }
                if let Ok(f) = t.parse::<f64>()
                    && f.fract() == 0.0
                    && f.abs() < 9e15
                {
                    return (f as i64).into();
                }
            }
            if has("number")
                && let Ok(f) = t.parse::<f64>()
                && let Some(n) = serde_json::Number::from_f64(f)
            {
                return Value::Number(n);
            }
            if has("boolean") {
                match t.to_ascii_lowercase().as_str() {
                    "true" | "yes" => return true.into(),
                    "false" | "no" => return false.into(),
                    _ => {}
                }
            }
            if (has("object") || has("array"))
                && let Ok(parsed) = serde_json::from_str::<Value>(t)
                && (parsed.is_object() || parsed.is_array())
            {
                return coerce_input(parsed, schema);
            }
            Value::String(s)
        }
        Value::Number(n) if has("string") && !has("number") && !has("integer") => {
            Value::String(n.to_string())
        }
        Value::Number(n) if has("integer") && !has("number") && n.as_i64().is_none() => {
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => (f as i64).into(),
                _ => Value::Number(n),
            }
        }
        Value::Object(mut o) => {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (k, sub) in props {
                    if let Some(x) = o.remove(k) {
                        o.insert(k.clone(), coerce_input(x, sub));
                    }
                }
            }
            Value::Object(o)
        }
        Value::Array(a) => match schema.get("items") {
            Some(items) => Value::Array(a.into_iter().map(|x| coerce_input(x, items)).collect()),
            None => Value::Array(a),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerces_what_small_models_send() {
        let schema = json!({
            "type": "object",
            "properties": {
                "wait_seconds": { "type": "integer" },
                "ratio": { "type": "number" },
                "force": { "type": "boolean" },
                "id": { "type": "string" },
                "edits": { "type": "array", "items": { "type": "object", "properties": { "n": { "type": "integer" } } } },
                "opt": { "type": ["integer", "null"] },
            }
        });
        let got = coerce_input(
            json!({
                "wait_seconds": "30", "ratio": "0.5", "force": "true", "id": 1002,
                "edits": "[{\"n\": \"2\"}]", "opt": "7", "extra": "kept"
            }),
            &schema,
        );
        assert_eq!(
            got,
            json!({
                "wait_seconds": 30, "ratio": 0.5, "force": true, "id": "1002",
                "edits": [{ "n": 2 }], "opt": 7, "extra": "kept"
            })
        );
        // The whole input as a JSON string; values that do not fit stay as they are.
        assert_eq!(
            coerce_input(json!("{\"wait_seconds\": \"5\"}"), &schema),
            json!({ "wait_seconds": 5 })
        );
        assert_eq!(
            coerce_input(json!({ "wait_seconds": "soon" }), &schema),
            json!({ "wait_seconds": "soon" })
        );
        assert_eq!(
            coerce_input(json!({ "wait_seconds": 3.0 }), &schema),
            json!({ "wait_seconds": 3 })
        );
    }
}
