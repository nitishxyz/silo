//! Kimi Code (the Kimi subscription's coding endpoint): Chat Completions with
//! `thinking: {type}` plus `reasoning_effort`, a fixed temperature, a prompt
//! cache key, reasoning replay, and the kimi-cli client headers. Also the
//! model list (`GET /models`) and plan usage (`GET /usages`).
use super::{Dialect, ReasoningParam, Replay, Wire, convert};
pub use crate::catalog::remote::RemoteModel;
use crate::{Caps, Effort, LlmError, ModelInfo, Reasoning, Request};
use serde_json::{Map, Value, json};

pub const ID: &str = "kimi-code";
pub const BASE_URL: &str = "https://api.kimi.ai/coding/v1";
/// The kimi-cli release these requests identify as (`X-Msh-Version`, `KimiCLI/<version>`).
pub const VERSION: &str = "1.52.0";

/// The dialect row. Thinking is shaped by [`request`] (it needs the model's own
/// efforts), so the generic converter is told to send none.
pub(super) const DIALECT: Dialect = Dialect {
    id: ID,
    name: "Kimi Code",
    wire: Wire::Chat,
    base_url: BASE_URL,
    base_env: None,
    env: &[],
    key_required: true,
    reasoning: ReasoningParam::None,
    replay: Replay::ReasoningContent,
    usage_option: true,
    tool_stream: false,
    cache: true,
    discovery: None,
};

impl RemoteModel {
    /// A `GET /models` entry as the harness sees it (provider `kimi-code`).
    #[must_use]
    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            provider: ID.into(),
            name: self.name.clone(),
            caps: Caps {
                reasoning: self.reasoning,
                vision: self.image_input,
                context_window: self.context_window,
                max_output: self.max_output,
                ..super::base_caps(&DIALECT)
            },
            pricing: None,
        }
    }
}

fn effort(name: &str) -> Option<Effort> {
    Some(match name.to_ascii_lowercase().as_str() {
        "minimal" => Effort::Minimal,
        "low" => Effort::Low,
        "medium" => Effort::Medium,
        "high" => Effort::High,
        "xhigh" => Effort::Xhigh,
        "max" => Effort::Max,
        _ => return None,
    })
}

/// The models of a `GET /models` body; empty when it has no `data` list.
#[must_use]
pub fn parse_models(body: &Value) -> Vec<RemoteModel> {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = text(&m["id"])?;
            let mut efforts: Vec<Effort> = m["think_efforts"].as_array().into_iter().flatten().filter_map(|e| effort(e.as_str()?)).collect();
            efforts.sort();
            efforts.dedup();
            Some(RemoteModel {
                name: text(&m["display_name"]).unwrap_or_else(|| id.clone()),
                id,
                context_window: m["context_length"].as_u64().unwrap_or(0),
                max_output: m["max_output_tokens"].as_u64().unwrap_or(0),
                reasoning: m["supports_reasoning"].as_bool().unwrap_or(false),
                image_input: m["supports_image_in"].as_bool().unwrap_or(false),
                efforts,
            })
        })
        .collect()
}

/// Models that always think reject `thinking: disabled`.
fn always_thinks(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ["thinking", "reason", "k3.1-pro"].iter().any(|s| id.contains(s))
}

fn budget(tokens: u32) -> Effort {
    match tokens {
        0 => Effort::None,
        1..=1024 => Effort::Minimal,
        1025..=4096 => Effort::Low,
        4097..=16384 => Effort::Medium,
        16385..=32768 => Effort::High,
        _ => Effort::Max,
    }
}

/// `thinking` and `reasoning_effort`: nothing unless the model reasons and the
/// caller chose; off only for models that can be turned off; an effort the
/// model does not list goes to the nearest it does, and none when it lists none.
fn thinking(body: &mut Value, requested: Option<Reasoning>, model: &ModelInfo, efforts: &[Effort]) {
    let Some(requested) = requested.filter(|_| model.caps.reasoning) else { return };
    let wanted = match requested {
        Reasoning::Effort(e) => e,
        Reasoning::Budget(n) => budget(n),
    };
    if wanted == Effort::None && !always_thinks(&model.id) {
        body["thinking"] = json!({"type":"disabled"});
        return;
    }
    body["thinking"] = json!({"type":"enabled"});
    // An always-thinking model asked for none gets its lowest effort.
    let chosen = match wanted {
        _ if efforts.is_empty() => None,
        Effort::None => efforts.iter().min().copied(),
        wanted => Some(wanted.nearest(efforts)),
    };
    if let Some(chosen) = chosen {
        body["reasoning_effort"] = json!(chosen);
    }
}

const UNSUPPORTED_SCHEMA: &[&str] = &[
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "definitions",
    "examples",
    "title",
    "nullable",
    "format",
    "pattern",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "prefixItems",
    "propertyNames",
    "patternProperties",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "dependentSchemas",
    "dependentRequired",
    "unevaluatedProperties",
];

/// The smaller JSON Schema dialect Kimi's tool calls accept: `const` becomes a
/// one-value `enum`, `oneOf` becomes `anyOf`, type lists become `anyOf`, nulls
/// leave `enum`, and the keywords it rejects are dropped.
fn sanitize(schema: &Value) -> Value {
    let object = match schema {
        Value::Array(items) => return Value::Array(items.iter().map(sanitize).collect()),
        Value::Object(object) => object,
        other => return other.clone(),
    };
    let mut out = Map::new();
    for (key, value) in object {
        match (key.as_str(), value) {
            ("const", Value::String(_) | Value::Number(_)) => {
                out.insert("enum".into(), json!([value]));
            }
            ("const", _) => {}
            ("oneOf", Value::Array(branches)) if !branches.is_empty() => {
                out.insert("anyOf".into(), sanitize(value));
            }
            ("oneOf", _) => {}
            ("properties", Value::Object(properties)) => {
                out.insert(key.clone(), Value::Object(properties.iter().map(|(name, schema)| (name.clone(), sanitize(schema))).collect()));
            }
            (k, _) if UNSUPPORTED_SCHEMA.contains(&k) => {}
            ("type", Value::Array(types)) => {
                let types: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
                match types.as_slice() {
                    [] => {}
                    [one] => {
                        out.insert(key.clone(), json!(one));
                    }
                    many => {
                        out.insert("anyOf".into(), Value::Array(many.iter().map(|t| json!({"type":t})).collect()));
                    }
                }
            }
            ("enum", Value::Array(values)) => {
                out.insert(key.clone(), Value::Array(values.iter().filter(|v| !v.is_null()).cloned().collect()));
            }
            _ => {
                out.insert(key.clone(), sanitize(value));
            }
        }
    }
    Value::Object(out)
}

/// The `POST /chat/completions` body: the generic Chat body, then Kimi's rules.
///
/// # Errors
/// What [`convert::request`] rejects.
pub fn request(req: &Request, model: &ModelInfo, efforts: &[Effort], session: Option<&str>) -> Result<Value, LlmError> {
    let mut body = convert::request(req, &DIALECT)?;
    // The coding endpoint fixes the sampling temperature; a caller's is never sent.
    body["temperature"] = json!(1);
    if let Some(limit) = body.as_object_mut().and_then(|b| b.remove("max_tokens")) {
        body["max_completion_tokens"] = limit;
    }
    thinking(&mut body, req.reasoning, model, efforts);
    if !req.cache.disabled
        && let Some(key) = req.cache.key.as_deref().or(session)
    {
        body["prompt_cache_key"] = json!(key);
    }
    let replay = model.caps.reasoning && body["thinking"]["type"] != "disabled";
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        if message["role"] != "assistant" || message.get("tool_calls").is_none() {
            continue;
        }
        // A tool-call turn carries `reasoning_content` while thinking is on, empty when it had none, and no null `content`.
        if replay && message.get("reasoning_content").is_none() {
            message["reasoning_content"] = json!("");
        }
        if message["content"].is_null()
            && let Some(object) = message.as_object_mut()
        {
            object.remove("content");
        }
    }
    for tool in body["tools"].as_array_mut().into_iter().flatten() {
        if let Some(parameters) = tool["function"].get("parameters").map(sanitize) {
            tool["function"]["parameters"] = parameters;
        }
    }
    Ok(body)
}

#[cfg(feature = "client")]
pub use client::{KimiCodeConfig, KimiCodeModel, SPEC, fetch_kimi_models, fetch_kimi_usage, kimi_headers, parse_usage};
#[cfg(feature = "client")]
mod client {
    use super::{BASE_URL, ID, RemoteModel, VERSION, parse_models, request};
    use crate::{
        CallCx, Effort, ErrorClass, EventStream, LlmError, Model, ModelInfo, PlanUsage, PlanWindow, Request,
        auth::{
            Credential, CredentialSource,
            flows::{self, Device, Refresh, Rotation, SubscriptionSpec},
            oauth::TokenEncoding,
        },
        compat::stream,
        http::{HttpClient, RetryPolicy, byte_stream_with_idle_timeout, classify_status, network, provider_message, send_with_retry},
        plan_usage::{timestamp, unix_now, window_label},
    };
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::Value;
    use std::{
        ffi::CStr,
        io::Write,
        os::unix::fs::OpenOptionsExt,
        path::Path,
        sync::{Mutex, PoisonError},
        time::{Duration, Instant},
    };

    /// Kimi Code sign-in as kimi-cli does it: an RFC 8628 device flow without secret or scope,
    /// kimi-cli's headers on every token call, and refresh tokens that rotate (401/403: rejected).
    pub static SPEC: SubscriptionSpec = SubscriptionSpec {
        provider: ID,
        api_key: false,
        client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
        client_secret: None,
        authorize: "",
        console: None,
        token: "https://auth.kimi.com/api/oauth/token",
        scopes: &[],
        params: &[],
        nonce: false,
        callbacks: &[],
        device: Some(Device::Rfc8628 { url: "https://auth.kimi.com/api/oauth/device_authorization", params: &[] }),
        headers: Some(kimi_headers),
        exchange_encoding: TokenEncoding::Form,
        exchange_state: false,
        refresh: Refresh {
            encoding: TokenEncoding::Form,
            rotation: Rotation::Required,
            rejected: flows::rejected_on_auth_status,
            rejected_message: Some("Kimi Code sign-in expired. Sign in again."),
        },
        credential: flows::plain_credential,
        finish: None,
        plan: flows::no_plan,
    };
    /// `/usages` is asked at most this often, across models.
    const USAGE_EVERY: Duration = Duration::from_secs(60);
    static USAGE_AT: Mutex<Option<Instant>> = Mutex::new(None);
    /// Whether a usage poll is due now (and if so, it counts as done).
    fn usage_due(now: Instant) -> bool {
        let mut at = USAGE_AT.lock().unwrap_or_else(PoisonError::into_inner);
        if at.is_some_and(|at| now.saturating_duration_since(at) < USAGE_EVERY) {
            return false;
        }
        *at = Some(now);
        true
    }

    /// Header values are printable ASCII.
    fn ascii(value: &str) -> String {
        let value: String = value.chars().filter(|c| (' '..='~').contains(c)).collect();
        let value = value.trim();
        if value.is_empty() { "unknown".into() } else { value.into() }
    }

    struct Host {
        name: String,
        system: String,
        release: String,
        version: String,
        machine: String,
    }

    fn host() -> Host {
        // SAFETY: `uname` fills a zeroed `utsname` whose fields are NUL-terminated.
        let u = unsafe {
            let mut u: libc::utsname = std::mem::zeroed();
            libc::uname(&mut u);
            u
        };
        let field = |f: &[libc::c_char]| unsafe { CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
        Host {
            name: field(&u.nodename),
            system: field(&u.sysname),
            release: field(&u.release),
            version: field(&u.version),
            machine: field(&u.machine),
        }
    }

    /// kimi-cli's device model: `macOS 15.1 arm64`, `Linux 6.8.0 x86_64`.
    fn device_model(host: &Host) -> String {
        if host.system != "Darwin" {
            return format!("{} {} {}", host.system, host.release, host.machine);
        }
        let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").unwrap_or_default();
        let version = plist
            .split_once("<key>ProductVersion</key>")
            .and_then(|(_, rest)| rest.split_once("<string>")?.1.split_once("</string>"))
            .map(|(v, _)| v.trim().to_owned());
        format!("macOS {} {}", version.unwrap_or_else(|| host.release.clone()), host.machine)
    }

    /// The install's id, created once at `<dir>/kimi-device-id`.
    fn device_id(dir: &Path) -> Result<String, LlmError> {
        let io = |e: std::io::Error| LlmError::new(ErrorClass::Other, format!("kimi device id: {e}"));
        let path = dir.join("kimi-device-id");
        let read = || std::fs::read_to_string(&path).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        if let Some(id) = read() {
            return Ok(id);
        }
        std::fs::create_dir_all(dir).map_err(io)?;
        let id = format!("{:032x}", rand::random::<u128>());
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => file.write_all(id.as_bytes()).map(|()| id).map_err(io),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read().ok_or_else(|| io(e)),
            Err(e) => Err(io(e)),
        }
    }

    /// The headers every Kimi Code call carries, as kimi-cli sends them; the
    /// device id is created under `state_dir` on first use and then stays.
    ///
    /// # Errors
    /// The device id file cannot be read or written.
    pub fn kimi_headers(state_dir: &Path) -> Result<HeaderMap, LlmError> {
        let host = host();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-msh-platform", "kimi_cli".to_owned()),
            ("x-msh-version", VERSION.to_owned()),
            ("x-msh-device-name", host.name.clone()),
            ("x-msh-device-model", device_model(&host)),
            ("x-msh-os-version", host.version.clone()),
            ("x-msh-device-id", device_id(state_dir)?),
            ("user-agent", format!("KimiCLI/{VERSION}")),
        ] {
            let value = HeaderValue::from_str(&ascii(&value)).map_err(|e| LlmError::new(ErrorClass::Other, e.to_string()))?;
            headers.insert(HeaderName::from_static(name), value);
        }
        Ok(headers)
    }

    #[derive(Clone)]
    pub struct KimiCodeConfig {
        /// The API root (normally [`BASE_URL`]).
        pub base_url: String,
        pub auth: CredentialSource,
        /// From [`RemoteModel::info`].
        pub model: ModelInfo,
        /// The efforts the model lists (`RemoteModel::efforts`).
        pub efforts: Vec<Effort>,
        /// From [`kimi_headers`].
        pub headers: HeaderMap,
        pub http: HttpClient,
        pub retry: RetryPolicy,
        pub idle_timeout: Duration,
    }
    pub struct KimiCodeModel {
        config: KimiCodeConfig,
    }
    impl KimiCodeModel {
        #[must_use]
        pub fn new(config: KimiCodeConfig) -> Self {
            Self { config }
        }
        async fn send(&self, url: &str, body: &Value, credential: &Credential, cx: &CallCx) -> Result<reqwest::Response, LlmError> {
            let token = match credential {
                Credential::ApiKey { key } => key,
                Credential::OAuth { access, .. } => access,
            };
            send_with_retry(
                || self.config.http.client().post(url).headers(self.config.headers.clone()).bearer_auth(token).json(body),
                &self.config.retry,
                cx,
            )
            .await
        }
    }
    #[async_trait::async_trait]
    impl Model for KimiCodeModel {
        fn info(&self) -> &ModelInfo {
            &self.config.model
        }
        async fn stream(&self, mut req: Request, cx: CallCx) -> Result<EventStream, LlmError> {
            if req.model.is_empty() {
                req.model.clone_from(&self.config.model.id);
            }
            let body = request(&req, &self.config.model, &self.config.efforts, cx.session.as_deref())?;
            let url = format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'));
            let credential = self.config.auth.current().await?;
            let response = match self.send(&url, &body, &credential, &cx).await {
                Err(error) if error.status == Some(401) && matches!(credential, Credential::OAuth { .. }) => {
                    let fresh = self.config.auth.unauthorized(&credential).await?;
                    self.send(&url, &body, &fresh, &cx).await?
                }
                result => result?,
            };
            Ok(stream::stream(byte_stream_with_idle_timeout(response, self.config.idle_timeout, cx.cancel), req.model, ID))
        }
        /// The subscription's `/usages`, at most once a minute.
        async fn plan_usage(&self) -> Option<PlanUsage> {
            let Ok(Credential::OAuth { access, .. }) = self.config.auth.current().await else { return None };
            if !usage_due(Instant::now()) {
                return None;
            }
            match fetch_kimi_usage(self.config.http.client(), &access).await {
                Ok(usage) => Some(usage).filter(|u| !u.is_empty()),
                Err(error) => {
                    tracing::debug!(%error, "kimi usage");
                    None
                }
            }
        }
    }

    async fn get(http: &reqwest::Client, url: &str, bearer: &str) -> Result<Value, LlmError> {
        let response = http.get(url).bearer_auth(bearer).timeout(Duration::from_secs(20)).send().await.map_err(network)?;
        let (status, headers) = (response.status(), response.headers().clone());
        let text = response.text().await.map_err(network)?;
        if !status.is_success() {
            return Err(LlmError { message: provider_message(&text), ..classify_status(status, &headers, &text) });
        }
        serde_json::from_str(&text).map_err(|e| LlmError::new(ErrorClass::Other, format!("{url}: {e}")))
    }

    /// `GET /coding/v1/models`.
    ///
    /// # Errors
    /// Network and status failures, or a body without a model list.
    pub async fn fetch_kimi_models(http: &reqwest::Client, bearer: &str) -> Result<Vec<RemoteModel>, LlmError> {
        let body = get(http, &format!("{BASE_URL}/models"), bearer).await?;
        if !body["data"].is_array() {
            return Err(LlmError::new(ErrorClass::Other, "unexpected Kimi model list"));
        }
        Ok(parse_models(&body))
    }

    /// `GET /coding/v1/usages` as plan windows.
    ///
    /// # Errors
    /// Network and status failures.
    pub async fn fetch_kimi_usage(http: &reqwest::Client, bearer: &str) -> Result<PlanUsage, LlmError> {
        Ok(parse_usage(&get(http, &format!("{BASE_URL}/usages"), bearer).await?, unix_now()))
    }

    /// A number or a numeric string (Kimi sends limits as either).
    fn number(value: &Value) -> Option<f64> {
        value.as_f64().or_else(|| value.as_str()?.trim().parse().ok()).filter(|n| n.is_finite())
    }

    /// Share used (0–1) and reset of a `{limit, used | remaining, resetTime}` object.
    fn quota(detail: &Value) -> Option<(f32, Option<i64>)> {
        let limit = number(&detail["limit"]).filter(|l| *l > 0.0)?;
        let used = number(&detail["used"]).or_else(|| number(&detail["remaining"]).map(|r| limit - r))?;
        let reset = ["resetTime", "reset_at", "resetAt", "reset_time"].iter().find_map(|k| detail[*k].as_str()).and_then(timestamp);
        #[allow(clippy::cast_possible_truncation)]
        Some(((used / limit).clamp(0.0, 1.0) as f32, reset))
    }

    /// A limit's window id from its length: `5h`, `7d`, `90m`.
    fn window_name(window: &Value) -> Option<String> {
        let duration = number(&window["duration"]).filter(|d| *d > 0.0)?;
        let unit = window["timeUnit"].as_str().unwrap_or_default();
        let minutes = if unit.contains("MINUTE") {
            duration
        } else if unit.contains("HOUR") {
            duration * 60.0
        } else if unit.contains("DAY") {
            duration * 1440.0
        } else {
            duration / 60.0
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let minutes = minutes.round() as u64;
        Some(match minutes {
            0 => return None,
            m if m % 1440 == 0 => format!("{}d", m / 1440),
            m if m % 60 == 0 => format!("{}h", m / 60),
            m => format!("{m}m"),
        })
    }

    /// A `GET /usages` body: each of `limits` by its window (the 5-hour one),
    /// and `usage`, the plan's overall allowance, as `Weekly` when it resets
    /// within a week of `now` (legacy plans), else `Monthly`.
    #[must_use]
    pub fn parse_usage(body: &Value, now: i64) -> PlanUsage {
        let mut windows: Vec<PlanWindow> = body["limits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|limit| {
                let name = window_name(&limit["window"])?;
                let detail = if limit["detail"].is_object() { &limit["detail"] } else { limit };
                let (used, resets_at) = quota(detail)?;
                Some(PlanWindow { label: window_label(&name), name, used, resets_at })
            })
            .collect();
        if let Some((used, resets_at)) = quota(&body["usage"]) {
            let weekly = resets_at.is_some_and(|at| at - now <= 7 * 86_400);
            let (name, label) = if weekly { ("7d", "Weekly") } else { ("30d", "Monthly") };
            if !windows.iter().any(|w| w.name == name) {
                windows.push(PlanWindow { name: name.into(), label: label.into(), used, resets_at });
            }
        }
        let mut usage = PlanUsage::default();
        usage.merge(PlanUsage { windows });
        usage
    }

    #[cfg(test)]
    #[test]
    fn usage_is_polled_at_most_once_a_minute() {
        // Past any real poll in this process, so nothing else moves the clock.
        let start = Instant::now() + Duration::from_secs(3600);
        assert!(usage_due(start));
        assert!(!usage_due(start + Duration::from_secs(59)));
        assert!(usage_due(start + USAGE_EVERY));
        assert!(!usage_due(start + USAGE_EVERY));
    }
}
