//! `silo do <method> [--param value]… [--json PARAMS]`: flags generated
//! from the server's `api.describe`, so a newer server's methods work.

use anyhow::{Context, bail};
use serde_json::{Map, Value};

/// How a param's value is read from the command line.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Bool,
    Integer,
    Number,
    String,
    /// One of these strings.
    Enum(Vec<String>),
    /// Anything else, given as JSON.
    Json,
}

/// One param of a method's `params_schema`.
#[derive(Clone, Debug)]
struct Param {
    name: String,
    kind: Kind,
    required: bool,
    description: Option<String>,
}

fn resolve<'a>(schema: &'a Value, defs: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix("#/$defs/")) {
        Some(name) => defs.get(name).map_or(schema, |def| resolve(def, defs)),
        None => schema,
    }
}

fn kind(schema: &Value, defs: &Value) -> Kind {
    let schema = resolve(schema, defs);
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let strings: Vec<String> = values.iter().filter_map(Value::as_str).map(str::to_owned).collect();
        return if strings.len() == values.len() { Kind::Enum(strings) } else { Kind::Json };
    }
    if let Some(value) = schema.get("const").and_then(Value::as_str) {
        return Kind::Enum(vec![value.to_owned()]);
    }
    let variants = ["anyOf", "oneOf"].into_iter().find_map(|k| schema.get(k)?.as_array());
    if let Some(variants) = variants {
        let kinds: Vec<Kind> = variants.iter().filter(|v| v.get("type").and_then(Value::as_str) != Some("null")).map(|v| kind(v, defs)).collect();
        return match kinds.as_slice() {
            [one] => one.clone(),
            many if many.iter().all(|k| matches!(k, Kind::Enum(_))) => {
                Kind::Enum(many.iter().flat_map(|k| if let Kind::Enum(v) = k { v.clone() } else { Vec::new() }).collect())
            }
            _ => Kind::Json,
        };
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).filter(|t| *t != "null").collect(),
        _ => Vec::new(),
    };
    match types.as_slice() {
        ["boolean"] => Kind::Bool,
        ["integer"] => Kind::Integer,
        ["number"] => Kind::Number,
        ["string"] => Kind::String,
        _ => Kind::Json,
    }
}

/// The params of a `params_schema`, required ones first in schema order.
fn params(schema: &Value) -> Vec<Param> {
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else { return Vec::new() };
    let mut out: Vec<Param> = properties
        .iter()
        .map(|(name, sub)| Param {
            name: name.clone(),
            kind: kind(sub, &defs),
            required: required.contains(&name.as_str()),
            description: sub.get("description").and_then(Value::as_str).map(str::to_owned),
        })
        .collect();
    out.sort_by_key(|p| p.required.then(|| required.iter().position(|r| *r == p.name)).flatten().unwrap_or(usize::MAX));
    out
}

fn value(param: &Param, text: &str) -> anyhow::Result<Value> {
    let bad = || format!("--{} {text:?}: expected {}", flag(&param.name), describe(&param.kind));
    Ok(match &param.kind {
        Kind::Bool => Value::Bool(match text {
            "true" | "yes" | "on" | "1" => true,
            "false" | "no" | "off" | "0" => false,
            _ => bail!(bad()),
        }),
        Kind::Integer => text.parse::<i64>().map(Value::from).with_context(bad)?,
        Kind::Number => text.parse::<f64>().ok().filter(|n| n.is_finite()).map(Value::from).with_context(bad)?,
        Kind::String => Value::String(text.to_owned()),
        Kind::Enum(values) if values.iter().any(|v| v == text) => Value::String(text.to_owned()),
        Kind::Enum(_) => bail!(bad()),
        Kind::Json => serde_json::from_str(text).with_context(bad)?,
    })
}

fn flag(name: &str) -> String {
    name.replace('_', "-")
}

fn describe(kind: &Kind) -> String {
    match kind {
        Kind::Bool => "true or false".into(),
        Kind::Integer => "an integer".into(),
        Kind::Number => "a number".into(),
        Kind::String => "a string".into(),
        Kind::Enum(values) => format!("one of {}", values.join(", ")),
        Kind::Json => "JSON".into(),
    }
}

/// `silo do <method> --help`.
#[must_use]
pub fn usage(method: &str, description: &str, schema: &Value) -> String {
    let mut out = format!("{description}\n\nUsage: silo do {method}");
    let params = params(schema);
    for p in params.iter().filter(|p| p.required) {
        out.push_str(&format!(" <{}>", flag(&p.name)));
    }
    out.push_str(" [--param value]… [--json PARAMS]\n");
    if !params.is_empty() {
        out.push_str("\nParams:\n");
    }
    for p in &params {
        let value = match &p.kind {
            Kind::Bool => String::new(),
            Kind::Enum(values) => format!(" <{}>", values.join("|")),
            Kind::Json => " <json>".into(),
            other => format!(" <{}>", describe(other).trim_start_matches("a ").trim_start_matches("an ")),
        };
        let required = if p.required { " (required)" } else { "" };
        out.push_str(&format!("  --{}{value}{required}", flag(&p.name)));
        if let Some(description) = &p.description {
            out.push_str(&format!("  {}", description.lines().next().unwrap_or_default()));
        }
        out.push('\n');
    }
    out
}

/// The params object `args` give for a method with `schema`: `--json`
/// first, then flags (`--name value`, `--name=value`, a bare boolean
/// flag, `--name-json JSON`), then positional values for the required
/// params not given yet, in schema order.
///
/// # Errors
/// An unknown flag, a value of the wrong type, extra positional values or
/// a missing required param.
pub fn build(schema: &Value, args: &[String]) -> anyhow::Result<Value> {
    let params = params(schema);
    let find = |name: &str| params.iter().find(|p| flag(&p.name) == name || p.name == name);
    let mut out = Map::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(name) = arg.strip_prefix("--") else {
            positional.push(arg.clone());
            continue;
        };
        let (name, inline) = match name.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (name, None),
        };
        if name == "json" {
            let text = inline.or_else(|| args.get(i).cloned().inspect(|_| i += 1)).context("--json needs a JSON object")?;
            let Value::Object(given) = serde_json::from_str(&text).context("--json must be a JSON object")? else {
                bail!("--json must be a JSON object")
            };
            out.extend(given);
            continue;
        }
        let (param, json) = match find(name) {
            Some(param) => (param, false),
            None => match name.strip_suffix("-json").and_then(find) {
                Some(param) => (param, true),
                None => bail!("unknown flag --{name}; see --help"),
            },
        };
        let next_is_value = args.get(i).is_some_and(|next| !next.starts_with("--") && (param.kind != Kind::Bool || value(param, next).is_ok()));
        let text = match inline {
            Some(text) => text,
            None if param.kind == Kind::Bool && !json && !next_is_value => "true".to_owned(),
            None => args.get(i).cloned().inspect(|_| i += 1).with_context(|| format!("--{name} needs a value"))?,
        };
        let value = if json { serde_json::from_str(&text).with_context(|| format!("--{name}: invalid JSON"))? } else { value(param, &text)? };
        out.insert(param.name.clone(), value);
    }
    let given: Vec<String> = out.keys().cloned().collect();
    let mut missing = params.iter().filter(|p| p.required && !given.contains(&p.name));
    for text in positional {
        let Some(param) = missing.next() else { bail!("unexpected argument {text:?}; see --help") };
        out.insert(param.name.clone(), value(param, &text)?);
    }
    if let Some(param) = missing.next() {
        bail!("missing --{}; see --help", flag(&param.name));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema(method: &str) -> Value {
        silo_proto::api::describe(method).unwrap().params_schema
    }

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn flags_come_from_the_schema() {
        let step = schema("window.step");
        assert_eq!(build(&step, &args("--session-id s1 --delta 1")).unwrap(), json!({"session_id": "s1", "delta": 1}));
        assert_eq!(build(&step, &args("--session_id=s1 --delta=-1")).unwrap(), json!({"session_id": "s1", "delta": -1}));
        assert_eq!(build(&step, &args("s1 2")).unwrap(), json!({"session_id": "s1", "delta": 2}), "required params by position");
        assert!(build(&step, &args("--session-id s1")).unwrap_err().to_string().contains("missing --delta"));
        assert!(build(&step, &args("--session-id s1 --delta one")).unwrap_err().to_string().contains("an integer"));
        assert!(build(&step, &args("--nope 1")).unwrap_err().to_string().contains("unknown flag --nope"));
        assert!(build(&step, &args("s1 1 extra")).is_err());

        let full = schema("app.full_screen");
        assert_eq!(build(&full, &[]).unwrap(), json!({}));
        assert_eq!(build(&full, &args("--on")).unwrap(), json!({"on": true}), "a bare boolean flag");
        assert_eq!(build(&full, &args("--on false --client-id 3")).unwrap(), json!({"on": false, "client_id": 3}));
        let reload = schema("browser.reload");
        assert_eq!(
            build(&reload, &args("--ignore-cache b1")).unwrap(),
            json!({"ignore_cache": true, "block_id": "b1"}),
            "a bare flag, then a position"
        );

        let dir = schema("block.focus_dir");
        assert_eq!(build(&dir, &args("b1 left")).unwrap(), json!({"block_id": "b1", "direction": "left"}));
        assert!(build(&dir, &args("b1 sideways")).unwrap_err().to_string().contains("one of left, right, up, down"));

        let grow = schema("layout.grow");
        assert_eq!(build(&grow, &args("b1 -0.1")).unwrap(), json!({"block_id": "b1", "delta": -0.1}));

        let split = schema("block.split");
        let given = build(&split, &args("b1 horizontal --spawn-json {\"command\":[\"top\"]} --kind browser")).unwrap();
        assert_eq!(given, json!({"block_id": "b1", "axis": "horizontal", "spawn": {"command": ["top"]}, "kind": "browser"}));
        let merged = build(&split, &args("--json {\"block_id\":\"b1\",\"axis\":\"vertical\"} --axis horizontal")).unwrap();
        assert_eq!(merged, json!({"block_id": "b1", "axis": "horizontal"}), "flags override --json");
    }

    #[test]
    fn closes_take_force_from_the_schema() {
        for method in silo_cli::CLOSES {
            let usage = usage(method, "", &schema(method));
            assert!(usage.contains("--force  Close even when programs are running"), "{method}: {usage}");
        }
        assert_eq!(build(&schema("block.close"), &args("b1 --force")).unwrap(), json!({"block_id": "b1", "force": true}));
        assert_eq!(build(&schema("server.shutdown"), &[]).unwrap(), json!({}), "force defaults to false on the server");
    }

    #[test]
    fn every_method_s_example_round_trips_through_flags() {
        for (name, _) in silo_proto::control::METHODS {
            let d = silo_proto::api::describe(name).unwrap();
            let mut argv = Vec::new();
            for (key, value) in d.example["params"].as_object().into_iter().flatten() {
                argv.push(format!("--{}-json", flag(key)));
                argv.push(value.to_string());
            }
            let built = build(&d.params_schema, &argv).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(built, d.example["params"], "{name}");
            assert!(usage(name, &d.description, &d.params_schema).contains(&format!("silo do {name}")));
        }
    }
}
