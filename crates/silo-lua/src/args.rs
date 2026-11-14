//! Action arguments: the short type spec (`{ file = "string", n =
//! "number?" }`) as a JSON Schema, and checking args against a schema.

use serde_json::{Map, Value, json};

/// The short spec's type names.
const TYPES: [&str; 8] = ["string", "number", "integer", "boolean", "object", "array", "any", "null"];

/// The JSON Schema `spec` means: a schema as it is when it has a `type`,
/// else an object whose properties are type names (`?` for optional) or
/// schemas.
///
/// # Errors
/// A property whose spec is neither a known type name nor a schema.
pub fn schema(spec: &Value) -> Result<Value, String> {
    let Value::Object(fields) = spec else {
        return Err("args must be a table of argument types, or a JSON Schema".into());
    };
    if fields.contains_key("type") {
        return Ok(spec.clone());
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    for (name, field) in fields {
        let property = match field {
            Value::String(text) => {
                let (ty, optional) = text.strip_suffix('?').map_or((text.as_str(), false), |t| (t, true));
                if !TYPES.contains(&ty) {
                    return Err(format!("args.{name}: unknown type “{text}” (one of {}, with ? when optional)", TYPES.join(", ")));
                }
                if !optional {
                    required.push(Value::String(name.clone()));
                }
                if ty == "any" { json!({}) } else { json!({ "type": ty }) }
            }
            Value::Object(_) => {
                required.push(Value::String(name.clone()));
                field.clone()
            }
            _ => return Err(format!("args.{name}: a type name or a JSON Schema")),
        };
        properties.insert(name.clone(), property);
    }
    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        required.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        schema["required"] = Value::Array(required);
    }
    Ok(schema)
}

/// Check `value` against `schema` (the common draft 2020-12 keywords:
/// `type`, `enum`, `const`, `properties`, `required`,
/// `additionalProperties`, `items`, `minimum`, `maximum`, `minLength`,
/// `maxLength`, `minItems`, `maxItems`, `anyOf`, `oneOf`, `allOf`,
/// `not`; others are ignored).
///
/// # Errors
/// The first mismatch, as `args.path: what`.
pub fn check(schema: &Value, value: &Value) -> Result<(), String> {
    walk(schema, value, "args")
}

fn type_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "number" => v.is_number(),
        "string" => v.is_string(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}

fn walk(schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    let fail = |what: String| Err(format!("{at}: {what}"));
    match schema {
        Value::Bool(false) => return fail("not allowed".into()),
        Value::Object(_) => {}
        _ => return Ok(()),
    }
    if let Some(types) = schema.get("type") {
        let types: Vec<&str> = match types {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|t| type_matches(t, v)) {
            let got = match v {
                Value::Null => "nothing",
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::String(_) => "a string",
                Value::Array(_) => "an array",
                Value::Object(_) => "an object",
            };
            return fail(format!("expected {}, got {got}", types.join(" or ")));
        }
    }
    if schema.get("const").is_some_and(|c| c != v) {
        return fail(format!("must be {}", schema["const"]));
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(v)
    {
        let list: Vec<String> = choices.iter().map(Value::to_string).collect();
        return fail(format!("must be one of {}", list.join(", ")));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return fail(format!("must be at least {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return fail(format!("must be at most {max}"));
        }
    }
    if let Some(s) = v.as_str() {
        let len = s.chars().count() as u64;
        if schema.get("minLength").and_then(Value::as_u64).is_some_and(|min| len < min) {
            return fail("too short".into());
        }
        if schema.get("maxLength").and_then(Value::as_u64).is_some_and(|max| len > max) {
            return fail("too long".into());
        }
    }
    if let Some(object) = v.as_object() {
        for key in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                return fail(format!("{key} is required"));
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (key, item) in object {
            let path = format!("{at}.{key}");
            match properties.and_then(|p| p.get(key)) {
                Some(sub) => walk(sub, item, &path)?,
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => return fail(format!("unexpected {key}")),
                    Some(sub @ Value::Object(_)) => walk(sub, item, &path)?,
                    _ => {}
                },
            }
        }
    }
    if let Some(items) = v.as_array() {
        if schema.get("minItems").and_then(Value::as_u64).is_some_and(|min| (items.len() as u64) < min) {
            return fail("too few items".into());
        }
        if schema.get("maxItems").and_then(Value::as_u64).is_some_and(|max| (items.len() as u64) > max) {
            return fail("too many items".into());
        }
        if let Some(sub) = schema.get("items").filter(|s| s.is_object() || s.is_boolean()) {
            for (i, item) in items.iter().enumerate() {
                walk(sub, item, &format!("{at}[{}]", i + 1))?;
            }
        }
    }
    for sub in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        walk(sub, v, at)?;
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array)
        && !any.iter().any(|s| walk(s, v, at).is_ok())
    {
        return fail("matches none of the allowed forms".into());
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array)
        && one.iter().filter(|s| walk(s, v, at).is_ok()).count() != 1
    {
        return fail("must match exactly one of the allowed forms".into());
    }
    if schema.get("not").is_some_and(|s| walk(s, v, at).is_ok()) {
        return fail("matches a form that is not allowed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_short_spec_is_a_schema() {
        let s = schema(&json!({"file": "string", "ratio": "number?", "any": "any?"})).unwrap();
        assert_eq!(
            s,
            json!({"type": "object", "required": ["file"], "properties": {
                "file": {"type": "string"}, "ratio": {"type": "number"}, "any": {}
            }})
        );
        let own = json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}});
        assert_eq!(schema(&own).unwrap(), own);
        assert!(schema(&json!({"x": "strng"})).unwrap_err().contains("args.x"));
        assert!(schema(&json!({"x": 3})).is_err());
    }

    #[test]
    fn args_are_checked() {
        let s = schema(&json!({"file": "string", "n": "integer?", "on": "boolean?"})).unwrap();
        assert!(check(&s, &json!({"file": "a"})).is_ok());
        assert!(check(&s, &json!({"file": "a", "n": 2, "extra": 1})).is_ok(), "extras pass");
        assert_eq!(check(&s, &json!({})).unwrap_err(), "args: file is required");
        assert_eq!(check(&s, &json!({"file": 1})).unwrap_err(), "args.file: expected string, got a number");
        assert!(check(&s, &json!({"file": "a", "n": 1.5})).is_err());
        assert!(check(&s, &json!({"file": "a", "n": 2.0})).is_ok());
        assert!(check(&s, &json!([1])).is_err());
        let strict = json!({"type": "object", "properties": {"mode": {"enum": ["a", "b"]}}, "additionalProperties": false});
        assert!(check(&strict, &json!({"mode": "c"})).is_err());
        assert!(check(&strict, &json!({"other": 1})).is_err());
        assert!(check(&json!({}), &json!({"anything": [1]})).is_ok());
    }
}
