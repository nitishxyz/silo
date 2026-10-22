//! Make MCP tool names and input schemas acceptable to every provider
//! (Anthropic `input_schema`, OpenAI function `parameters`).
use serde_json::{Map, Value, json};

/// `mcp__<server>__<tool>`, limited to `[A-Za-z0-9_-]` and 64 characters
/// (a stable hash suffix keeps truncated names distinct).
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    if name.len() <= 64 {
        return name;
    }
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    format!("{}_{:08x}", &name[..55], hash as u32)
}

const TOP_LEVEL_FORBIDDEN: [&str; 9] = ["oneOf", "anyOf", "allOf", "not", "enum", "const", "if", "then", "else"];

/// A top-level `object` schema with object `properties`, `required` naming
/// only real properties, resolvable local `$ref`s, and `items` on every array.
/// Top-level combinators are folded: object variants' properties are merged
/// (none required), the rest dropped.
pub fn sanitize(schema: &Value) -> Value {
    let mut root = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    for key in ["$schema", "$id", "$comment"] {
        root.remove(key);
    }
    let mut properties = match root.remove("properties") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    for key in TOP_LEVEL_FORBIDDEN {
        let Some(removed) = root.remove(key).filter(|_| matches!(key, "oneOf" | "anyOf" | "allOf")) else { continue };
        for variant in removed.as_array().into_iter().flatten().chain(removed.is_object().then_some(&removed)) {
            for (name, property) in variant["properties"].as_object().into_iter().flatten() {
                properties.entry(name.clone()).or_insert_with(|| property.clone());
            }
        }
    }
    let required: Vec<Value> = root
        .remove("required")
        .and_then(|r| r.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.as_str().is_some_and(|name| properties.contains_key(name)))
        .collect();
    root.insert("type".into(), json!("object"));
    root.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        root.insert("required".into(), Value::Array(required));
    }
    let mut value = Value::Object(root);
    let snapshot = value.clone();
    repair(&mut value, &snapshot);
    value
}

/// Drop unresolvable `$ref`s (to `{}`) and give arrays `items`.
fn repair(value: &mut Value, root: &Value) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str)
                && !reference.strip_prefix('#').is_some_and(|pointer| pointer.is_empty() || root.pointer(pointer).is_some())
            {
                map.remove("$ref");
            }
            let is_array = map.get("type").is_some_and(|t| t == "array" || t.as_array().is_some_and(|ts| ts.iter().any(|t| t == "array")));
            if is_array && !map.contains_key("items") {
                map.insert("items".into(), json!({}));
            }
            for (key, child) in map.iter_mut() {
                // `properties` keys are names, not keywords; recurse into their schemas.
                if key == "properties" || key == "$defs" || key == "definitions" || key == "patternProperties" {
                    for schema in child.as_object_mut().into_iter().flat_map(|m| m.values_mut()) {
                        repair(schema, root);
                    }
                } else if !matches!(key.as_str(), "enum" | "const" | "default" | "examples" | "required") {
                    repair(child, root);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| repair(v, root)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_namespaced_and_bounded() {
        assert_eq!(tool_name("git hub", "create.issue"), "mcp__git_hub__create_issue");
        let long = tool_name("server", &"x".repeat(80));
        assert_eq!(long.len(), 64);
        assert_ne!(long, tool_name("server", &"x".repeat(81)));
    }

    #[test]
    fn hostile_schemas_become_safe() {
        let schema = sanitize(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "anyOf": [{"type":"object","properties":{"a":{"type":"string"}}}, {"type":"object","properties":{"b":{"type":"array"}}}],
            "required": ["a", "missing"],
            "properties": {"c": {"$ref": "#/$defs/nowhere"}, "d": {"$ref": "#/$defs/ok"}},
            "$defs": {"ok": {"type": "array"}}
        }));
        assert_eq!(schema["type"], "object");
        assert!(schema.get("anyOf").is_none() && schema.get("$schema").is_none());
        assert_eq!(schema["required"], json!(["a"]));
        assert_eq!(schema["properties"]["b"]["items"], json!({}));
        assert!(schema["properties"]["c"].get("$ref").is_none());
        assert_eq!(schema["properties"]["d"]["$ref"], "#/$defs/ok");
        assert_eq!(schema["$defs"]["ok"]["items"], json!({}));
        assert_eq!(sanitize(&json!(null)), json!({"type":"object","properties":{}}));
    }
}
