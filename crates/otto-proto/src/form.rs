//! Questions with fields: an MCP server's elicitation (`elicitation/create`, form mode)
//! as otto shows it. [`QuestionForm::from_schema`] reads the server's flat
//! `requestedSchema`; [`QuestionForm::validate`] checks an answer against it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{future::Future, pin::Pin};

/// How the user answered a question with fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAction {
    /// Submitted (the fields' values in `content`).
    #[default]
    Accept,
    /// Said no.
    Decline,
    /// Dismissed without choosing (also a stopped turn or a timeout).
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Number,
    Integer,
    Boolean,
    /// One of `choices`.
    Choice,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FieldChoice {
    pub value: String,
    /// Shown instead of `value` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl FieldChoice {
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.value)
    }
}

/// One input: a property of the requested schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FormField {
    /// The property name (the key in `content`).
    pub name: String,
    pub kind: FieldKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Prefilled value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<FieldChoice>,
    /// Text: `email`, `uri`, `date` or `date-time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
}

/// A question's fields, in the schema's order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct QuestionForm {
    pub fields: Vec<FormField>,
}

/// What an MCP server asks (otto-mcp to the session).
#[derive(Clone, Debug)]
pub struct Elicitation {
    /// The MCP server's name.
    pub server: String,
    pub message: String,
    pub form: QuestionForm,
}

/// The user's reply to an [`Elicitation`]; `content` only on accept.
#[derive(Clone, Debug, PartialEq)]
pub struct FormReply {
    pub action: QuestionAction,
    pub content: Option<Map<String, Value>>,
}

impl FormReply {
    pub fn decline() -> Self {
        Self { action: QuestionAction::Decline, content: None }
    }
    pub fn cancel() -> Self {
        Self { action: QuestionAction::Cancel, content: None }
    }
}

/// Asks the calling session's user: otto-core puts one in a tool call's
/// `ToolCx::services` (otto-mcp downcasts it). Missing: nobody can answer.
#[allow(clippy::type_complexity)]
pub struct Elicitor(Box<dyn Fn(Elicitation) -> Pin<Box<dyn Future<Output = FormReply> + Send>> + Send + Sync>);

impl Elicitor {
    pub fn new<F, Fut>(ask: F) -> Self
    where
        F: Fn(Elicitation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FormReply> + Send + 'static,
    {
        Self(Box::new(move |elicitation| Box::pin(ask(elicitation))))
    }

    pub fn ask(&self, elicitation: Elicitation) -> impl Future<Output = FormReply> + Send + use<> {
        (self.0)(elicitation)
    }
}

impl FormField {
    /// The title, else the property name.
    pub fn label(&self) -> &str {
        self.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or(&self.name)
    }

    /// The default as an input shows it (text, number or a choice's value).
    pub fn default_text(&self) -> String {
        match &self.default {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => if *b { "yes" } else { "no" }.into(),
            _ => String::new(),
        }
    }

    /// Typed text as this field's value; `None` for blank text.
    pub fn parse(&self, text: &str) -> Result<Option<Value>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let label = self.label();
        let value = match self.kind {
            FieldKind::Text => Value::String(text.into()),
            FieldKind::Integer => {
                let n: f64 = text.parse().map_err(|_| format!("{label}: enter a whole number"))?;
                if n.fract() != 0.0 || !n.is_finite() {
                    return Err(format!("{label}: enter a whole number"));
                }
                Value::from(n as i64)
            }
            FieldKind::Number => {
                let n: f64 = text.parse().map_err(|_| format!("{label}: enter a number"))?;
                serde_json::Number::from_f64(n).map(Value::Number).ok_or_else(|| format!("{label}: enter a number"))?
            }
            FieldKind::Boolean => match text.to_ascii_lowercase().as_str() {
                "yes" | "y" | "true" | "on" | "1" => Value::Bool(true),
                "no" | "n" | "false" | "off" | "0" => Value::Bool(false),
                _ => return Err(format!("{label}: answer yes or no")),
            },
            FieldKind::Choice => {
                let choice = self
                    .choices
                    .iter()
                    .find(|c| c.value == text)
                    .or_else(|| self.choices.iter().find(|c| c.value.eq_ignore_ascii_case(text) || c.label().eq_ignore_ascii_case(text)))
                    .ok_or_else(|| format!("{label}: choose one of {}", self.choice_labels().join(", ")))?;
                Value::String(choice.value.clone())
            }
        };
        Ok(Some(value))
    }

    fn choice_labels(&self) -> Vec<&str> {
        self.choices.iter().map(FieldChoice::label).collect()
    }

    /// `value` fits this field; returns it normalized (an integral number for integers).
    pub fn check(&self, value: &Value) -> Result<Value, String> {
        let label = self.label();
        match self.kind {
            FieldKind::Text => {
                let text = value.as_str().ok_or_else(|| format!("{label}: expected text"))?;
                let chars = text.chars().count() as u64;
                if let Some(min) = self.min_length.filter(|min| chars < *min) {
                    return Err(format!("{label}: at least {min} characters"));
                }
                if let Some(max) = self.max_length.filter(|max| chars > *max) {
                    return Err(format!("{label}: at most {max} characters"));
                }
                if let Some(format) = &self.format
                    && !format_ok(format, text)
                {
                    return Err(format!("{label}: not a valid {}", format_name(format)));
                }
                Ok(value.clone())
            }
            FieldKind::Number | FieldKind::Integer => {
                let n = value.as_f64().ok_or_else(|| format!("{label}: expected a number"))?;
                let value = if self.kind == FieldKind::Integer {
                    if n.fract() != 0.0 {
                        return Err(format!("{label}: expected a whole number"));
                    }
                    Value::from(n as i64)
                } else {
                    value.clone()
                };
                if let Some(min) = self.minimum.filter(|min| n < *min) {
                    return Err(format!("{label}: at least {min}"));
                }
                if let Some(max) = self.maximum.filter(|max| n > *max) {
                    return Err(format!("{label}: at most {max}"));
                }
                Ok(value)
            }
            FieldKind::Boolean => value.as_bool().map(Value::Bool).ok_or_else(|| format!("{label}: expected yes or no")),
            FieldKind::Choice => match value.as_str() {
                Some(v) if self.choices.iter().any(|c| c.value == v) => Ok(value.clone()),
                _ => Err(format!("{label}: choose one of {}", self.choice_labels().join(", "))),
            },
        }
    }
}

fn format_name(format: &str) -> &str {
    match format {
        "uri" => "URL",
        "date-time" => "date and time",
        other => other,
    }
}

fn is_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    let num = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    parts.len() == 3
        && num(parts[0], 4)
        && num(parts[1], 2)
        && num(parts[2], 2)
        && (1..=12).contains(&parts[1].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&parts[2].parse::<u32>().unwrap_or(0))
}

/// A light check of the formats elicitation allows; unknown formats pass.
fn format_ok(format: &str, text: &str) -> bool {
    match format {
        "email" => text.split_once('@').is_some_and(|(user, host)| {
            !user.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !text.contains(char::is_whitespace)
        }),
        "uri" => text.split_once(':').is_some_and(|(scheme, rest)| {
            scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                && !rest.is_empty()
                && !text.contains(char::is_whitespace)
        }),
        "date" => is_date(text),
        "date-time" => text.split_once(['T', 't', ' ']).is_some_and(|(date, time)| is_date(date) && time.len() >= 5 && time.as_bytes()[2] == b':'),
        _ => true,
    }
}

fn number(schema: &Value, key: &str) -> Option<f64> {
    schema.get(key).and_then(Value::as_f64)
}

impl QuestionForm {
    /// The fields of an elicitation's `requestedSchema`: an object whose properties are
    /// strings (optionally `enum`, `oneOf` of `const`s, or a `format`), numbers, integers
    /// or booleans. Anything else is an error naming the property.
    pub fn from_schema(schema: &Value) -> Result<Self, String> {
        if schema.is_null() {
            return Ok(Self::default());
        }
        if schema.get("type").is_some_and(|t| t != "object") {
            return Err("requestedSchema must be an object".into());
        }
        let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut fields = Vec::new();
        for (name, property) in schema["properties"].as_object().into_iter().flatten() {
            let text = |key: &str| property.get(key).and_then(Value::as_str).map(str::to_owned);
            let mut field = FormField {
                name: name.clone(),
                kind: FieldKind::Text,
                title: text("title"),
                description: text("description"),
                required: required.contains(&name.as_str()),
                default: property.get("default").filter(|d| !d.is_null()).cloned(),
                choices: Vec::new(),
                format: None,
                min_length: property.get("minLength").and_then(Value::as_u64),
                max_length: property.get("maxLength").and_then(Value::as_u64),
                minimum: number(property, "minimum"),
                maximum: number(property, "maximum"),
            };
            let kind = property["type"].as_str();
            if let Some(values) = property["enum"].as_array() {
                let labels = property["enumNames"].as_array();
                field.kind = FieldKind::Choice;
                field.choices = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| FieldChoice {
                        value: v.as_str().map_or_else(|| v.to_string(), str::to_owned),
                        label: labels.and_then(|l| l.get(i)).and_then(Value::as_str).map(str::to_owned),
                    })
                    .collect();
            } else if let Some(options) = property["oneOf"].as_array().filter(|_| kind.is_none_or(|k| k == "string")) {
                field.kind = FieldKind::Choice;
                field.choices = options
                    .iter()
                    .filter_map(|o| {
                        let value = o.get("const")?;
                        Some(FieldChoice {
                            value: value.as_str().map_or_else(|| value.to_string(), str::to_owned),
                            label: o.get("title").and_then(Value::as_str).map(str::to_owned),
                        })
                    })
                    .collect();
            } else {
                field.kind = match kind {
                    Some("string") => FieldKind::Text,
                    Some("number") => FieldKind::Number,
                    Some("integer") => FieldKind::Integer,
                    Some("boolean") => FieldKind::Boolean,
                    other => return Err(format!("{name}: unsupported field type {}", other.unwrap_or("(none)"))),
                };
                if field.kind == FieldKind::Text {
                    field.format = text("format");
                }
            }
            if field.kind == FieldKind::Choice && field.choices.is_empty() {
                return Err(format!("{name}: a choice without options"));
            }
            // A default that does not fit is dropped rather than shown.
            if let Some(default) = &field.default
                && field.check(default).is_err()
            {
                field.default = None;
            }
            fields.push(field);
        }
        Ok(Self { fields })
    }

    pub fn field(&self, name: &str) -> Option<&FormField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The content to send: each known value checked (null and blank text left out),
    /// required fields present, unknown keys dropped.
    pub fn validate(&self, content: &Map<String, Value>) -> Result<Map<String, Value>, String> {
        let mut out = Map::new();
        let mut problems = Vec::new();
        for field in &self.fields {
            let value = content.get(&field.name).filter(|v| !v.is_null() && v.as_str().is_none_or(|s| !s.trim().is_empty()));
            match value {
                None if field.required => problems.push(format!("{} is required", field.label())),
                None => {}
                Some(value) => match field.check(value) {
                    Ok(value) => {
                        out.insert(field.name.clone(), value);
                    }
                    Err(problem) => problems.push(problem),
                },
            }
        }
        if problems.is_empty() { Ok(out) } else { Err(problems.join("; ")) }
    }

    /// A plain-text answer (an older client, a sub-agent's parent, the CLI): a JSON
    /// object of values, else the value of a form's single field; a form without
    /// fields takes any answer.
    pub fn text_answer(&self, text: &str) -> Result<Map<String, Value>, String> {
        if let Ok(Value::Object(content)) = serde_json::from_str::<Value>(text.trim()) {
            return self.validate(&content);
        }
        match self.fields.as_slice() {
            [] => Ok(Map::new()),
            [field] => {
                let mut content = Map::new();
                if let Some(value) = field.parse(text)? {
                    content.insert(field.name.clone(), value);
                }
                self.validate(&content)
            }
            fields => {
                let names: Vec<String> = fields.iter().map(|f| format!("\"{}\": …", f.name)).collect();
                Err(format!("Answer with a JSON object: {{{}}}", names.join(", ")))
            }
        }
    }

    /// Suggested answers for clients that show only text and options: a single
    /// choice's labels, `Yes`/`No` for a single checkbox.
    pub fn options(&self) -> Vec<String> {
        match self.fields.as_slice() {
            [field] if field.kind == FieldKind::Choice => field.choices.iter().map(|c| c.label().to_owned()).collect(),
            [field] if field.kind == FieldKind::Boolean => vec!["Yes".into(), "No".into()],
            _ => Vec::new(),
        }
    }

    /// `Name: Ann · Age: 3`, for the transcript and the resolved event.
    pub fn summary(&self, content: &Map<String, Value>) -> String {
        let shown = |field: &FormField, value: &Value| match (field.kind, value) {
            (FieldKind::Boolean, Value::Bool(b)) => if *b { "yes" } else { "no" }.to_owned(),
            (FieldKind::Choice, Value::String(v)) => field.choices.iter().find(|c| c.value == *v).map_or_else(|| v.clone(), |c| c.label().to_owned()),
            (_, Value::String(s)) => s.clone(),
            (_, other) => other.to_string(),
        };
        let parts: Vec<String> = self.fields.iter().filter_map(|f| content.get(&f.name).map(|v| format!("{}: {}", f.label(), shown(f, v)))).collect();
        if parts.is_empty() { "Submitted".into() } else { parts.join(" · ") }
    }
}
