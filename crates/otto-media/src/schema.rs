//! Input schemas (docs/MEDIA.md §3.2): trimmed for the model, their media fields found, inputs checked.
use serde_json::{Map, Value};

/// What a media field takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Media {
    Image,
    Video,
    Audio,
    File,
}

impl Media {
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::File => "file",
        }
    }
}

/// A media field: `image_urls`, `elements[].frontal_image_url`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Field {
    pub name: String,
    pub media: Media,
    /// An array: its item bounds.
    pub many: Option<(Option<u64>, Option<u64>)>,
    pub required: bool,
}

impl Field {
    /// `video_url`, `image_urls 1–8`, `audio_urls ≤10?`, `end_image_url?`.
    pub(crate) fn short(&self) -> String {
        // An optional list's `minItems: 1` only says it isn't empty when given.
        let least = |min: u64| min > 1 || (min == 1 && self.required);
        let count = match self.many {
            Some((Some(min), Some(max))) if min == max => format!(" {min}"),
            Some((Some(min), Some(max))) if least(min) => format!(" {min}–{max}"),
            Some((_, Some(max))) => format!(" ≤{max}"),
            Some((Some(min), None)) if least(min) => format!(" ≥{min}"),
            _ => String::new(),
        };
        format!("{}{count}{}", self.name, if self.required { "" } else { "?" })
    }
}

/// The non-null branch of `anyOf: [X, {type: null}]` (fal's optional fields), else the node.
fn plain(node: &Value) -> &Value {
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = node[key].as_array()
            && let Some(branch) = branches.iter().find(|b| b["type"] != "null")
        {
            return branch;
        }
    }
    node
}

fn is_string(node: &Value) -> bool {
    node["type"] == "string" || node["type"].as_array().is_some_and(|t| t.iter().any(|t| t == "string"))
}

/// A string field taking media: `format: uri`, a media `contentMediaType`, fal's `ui.field`, or a `…_url(s)` name.
fn media_of(name: &str, node: &Value) -> Option<Media> {
    if !is_string(node) {
        return None;
    }
    let hint = node["contentMediaType"]
        .as_str()
        .and_then(|m| m.split('/').next())
        .or_else(|| node["ui"]["field"].as_str())
        .or_else(|| node["_fal_ui_field"].as_str());
    let named = ["_url", "_urls", "_uri"].iter().any(|s| name.ends_with(s));
    if node["format"] != "uri" && hint.is_none() && !named {
        return None;
    }
    let name = name.to_lowercase();
    Some(match hint {
        Some("image") => Media::Image,
        Some("video") => Media::Video,
        Some("audio") => Media::Audio,
        _ if name.contains("video") => Media::Video,
        _ if ["audio", "voice", "music", "speech", "sound"].iter().any(|w| name.contains(w)) => Media::Audio,
        _ if ["image", "frame", "mask", "photo", "face", "style", "reference"].iter().any(|w| name.contains(w)) => Media::Image,
        _ => Media::File,
    })
}

/// The media fields of an object schema, nested ones too: required first, then by name.
pub(crate) fn fields(schema: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    collect(schema, "", &mut out);
    out.sort_by(|a, b| (!a.required, &a.name).cmp(&(!b.required, &b.name)));
    out
}

fn collect(node: &Value, path: &str, out: &mut Vec<Field>) {
    let required: Vec<&str> = node["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    for (name, property) in node["properties"].as_object().into_iter().flatten() {
        let (property, at) = (plain(property), if path.is_empty() { name.clone() } else { format!("{path}.{name}") });
        let required = required.contains(&name.as_str());
        if let Some(media) = media_of(name, property) {
            out.push(Field { name: at, media, many: None, required });
        } else if property["type"] == "array" {
            let item = plain(&property["items"]);
            match media_of(name, item) {
                Some(media) => {
                    let many = Some((property["minItems"].as_u64(), property["maxItems"].as_u64()));
                    out.push(Field { name: at, media, many, required });
                }
                None if item["properties"].is_object() => collect(item, &format!("{at}[]"), out),
                None => {}
            }
        } else if property["properties"].is_object() {
            collect(property, &at, out);
        }
    }
}

/// Where `input` holds media (JSON pointers) and what each takes.
pub(crate) fn locate(input: &Value, schema: &Value) -> Vec<(String, Media)> {
    let mut out = Vec::new();
    walk(input, schema, "", String::new(), &mut out);
    out
}

fn walk(value: &Value, node: &Value, name: &str, at: String, out: &mut Vec<(String, Media)>) {
    let node = plain(node);
    match value {
        Value::String(_) => {
            if let Some(media) = media_of(name, node) {
                out.push((at, media));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &node["items"], name, format!("{at}/{i}"), out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                walk(item, &node["properties"][key], key, format!("{at}/{}", key.replace('~', "~0").replace('/', "~1")), out);
            }
        }
        _ => {}
    }
}

/// Keys the model doesn't need: provider UI hints, examples, titles.
fn internal(key: &str) -> bool {
    key.starts_with("x-") || key.starts_with('_') || matches!(key, "examples" | "ui" | "title")
}

/// `schema` without internal keys, fal's `anyOf: [X, null]` as `X`, and `$ref`s into `root` inlined.
pub(crate) fn trim(schema: &Value, root: &Value) -> Value {
    trimmed(schema, root, 0)
}

fn trimmed(node: &Value, root: &Value, depth: usize) -> Value {
    match node {
        Value::Object(map) if depth < 12 => {
            if let Some(target) = map.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')).and_then(|p| root.pointer(p)) {
                let mut inlined = trimmed(target, root, depth + 1);
                // Siblings of a `$ref` (its description) stay.
                for (k, v) in map.iter().filter(|(k, _)| *k != "$ref" && !internal(k)) {
                    inlined[k] = trimmed(v, root, depth + 1);
                }
                return inlined;
            }
            if let Some(branches) = map.get("anyOf").and_then(Value::as_array) {
                let real: Vec<&Value> = branches.iter().filter(|b| b["type"] != "null").collect();
                if let [one] = real.as_slice() {
                    let mut merged = trimmed(one, root, depth + 1);
                    for (k, v) in map.iter().filter(|(k, _)| *k != "anyOf" && !internal(k)) {
                        merged[k] = trimmed(v, root, depth + 1);
                    }
                    return merged;
                }
            }
            let mut out = Map::new();
            for (k, v) in map.iter().filter(|(k, _)| !internal(k)) {
                // `properties` holds field names, not keywords: keep every one.
                let v = if k == "properties" {
                    Value::Object(v.as_object().into_iter().flatten().map(|(n, p)| (n.clone(), trimmed(p, root, depth + 1))).collect())
                } else {
                    trimmed(v, root, depth + 1)
                };
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| trimmed(i, root, depth + 1)).collect()),
        other => other.clone(),
    }
}

/// `input` checked against `schema` (formats aren't: media fields hold paths and ids until uploaded): the first
/// few errors, each at its field.
pub(crate) fn validate(schema: &Value, input: &Value) -> Result<(), String> {
    let validator = jsonschema::options().should_validate_formats(false).build(schema).map_err(|e| format!("its schema is unreadable: {e}"))?;
    let errors: Vec<String> = validator
        .iter_errors(input)
        .take(4)
        .map(|e| {
            let at = e.instance_path().to_string().trim_start_matches('/').replace('/', ".");
            // A long value (a prompt, a list of links) reads as `the value`.
            let value = e.instance().to_string();
            let message = e.to_string();
            let message = if value.len() > 60 { message.replacen(&value, "the value", 1) } else { message };
            let message: String = message.chars().take(300).collect();
            if at.is_empty() { message } else { format!("{at}: {message}") }
        })
        .collect();
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}
