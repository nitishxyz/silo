//! How much of a subscription's limits is used (Claude's 5-hour and weekly
//! windows, ChatGPT's Codex windows), from response headers and stream
//! events (docs/OTTO.md §4.9). API keys and local models report none.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanUsage {
    pub windows: Vec<PlanWindow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlanWindow {
    /// The provider's id for it: `5h`, `7d`, `7d_opus`, `primary`, `secondary`.
    pub name: String,
    /// As shown: `5-hour`, `Weekly`, `Weekly Opus`, `{n}h`, `{n}d`.
    pub label: String,
    /// Share used, 0–1.
    pub used: f32,
    /// When it resets (seconds since the Unix epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

impl PlanUsage {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
    /// The window closest to its limit.
    #[must_use]
    pub fn tightest(&self) -> Option<&PlanWindow> {
        self.windows.iter().max_by(|a, b| a.used.total_cmp(&b.used))
    }
    /// `newer`'s windows replace those of the same name; the rest stay.
    pub fn merge(&mut self, newer: PlanUsage) {
        for window in newer.windows {
            match self.windows.iter_mut().find(|w| w.name == window.name) {
                Some(old) => *old = window,
                None => self.windows.push(window),
            }
        }
        sort(&mut self.windows);
    }
}

/// A window's label from its length: `5-hour`, `Weekly`, else `{n}d` / `{n}h` / `{n}m`.
#[must_use]
pub fn label(minutes: u64) -> String {
    match minutes {
        300 => "5-hour".into(),
        10_080 => "Weekly".into(),
        m if m >= 1440 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m >= 60 => format!("{}h", (m + 30) / 60),
        m => format!("{m}m"),
    }
}

/// A window id's length (`5h`, `7d`, `7d_opus`) and what follows it (`opus`).
fn minutes(name: &str) -> Option<(u64, &str)> {
    let digits = name.find(|c: char| !c.is_ascii_digit())?;
    let n: u64 = name[..digits].parse().ok()?;
    let mut rest = name[digits..].chars();
    let unit = match rest.next()? {
        'm' => 1,
        'h' => 60,
        'd' => 1440,
        'w' => 10_080,
        _ => return None,
    };
    let rest = rest.as_str();
    (rest.is_empty() || rest.starts_with(['_', '-'])).then(|| (n * unit, rest.trim_start_matches(['_', '-'])))
}

fn capitalize(s: &str) -> String {
    let s = s.replace(['_', '-'], " ");
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// The label of a window named by its length (`7d_opus` → `Weekly Opus`), else its name.
#[must_use]
pub fn window_label(name: &str) -> String {
    match minutes(name) {
        Some((m, "")) => label(m),
        Some((m, rest)) => format!("{} {}", label(m), capitalize(rest)),
        None => capitalize(name),
    }
}

/// Shortest first, then by name.
fn sort(windows: &mut [PlanWindow]) {
    windows.sort_by(|a, b| {
        let len = |w: &PlanWindow| minutes(&w.name).map_or(u64::MAX, |m| m.0);
        len(a).cmp(&len(b)).then_with(|| a.name.cmp(&b.name))
    });
}

#[cfg(feature = "client")]
pub use parse::*;
#[cfg(feature = "client")]
mod parse {
    use super::{PlanUsage, PlanWindow, capitalize, label, sort, window_label};
    use crate::{EventStream, StreamEvent};
    use serde_json::Value;

    /// Seconds since the Unix epoch.
    #[must_use]
    pub fn unix_now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }

    /// `0.62` or `62` (a percent, above 1) as 0–1.
    fn fraction(value: &str) -> Option<f32> {
        let n: f64 = value.trim().parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
        #[allow(clippy::cast_possible_truncation)]
        Some((if n > 1.0 { n / 100.0 } else { n }).min(1.0) as f32)
    }

    /// A percent (`42`, `42.5`) as 0–1.
    fn percent(value: f64) -> Option<f32> {
        #[allow(clippy::cast_possible_truncation)]
        (value.is_finite() && value >= 0.0).then(|| (value / 100.0).min(1.0) as f32)
    }

    /// Unix seconds (milliseconds when that large) or RFC 3339.
    fn timestamp(value: &str) -> Option<i64> {
        let value = value.trim();
        if let Ok(n) = value.parse::<f64>() {
            #[allow(clippy::cast_possible_truncation)]
            let n = n as i64;
            return (n > 0).then_some(if n > 100_000_000_000 { n / 1000 } else { n });
        }
        chrono::DateTime::parse_from_rfc3339(value).ok().map(|d| d.timestamp())
    }

    fn lower<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<(String, &'a str)> {
        headers.into_iter().map(|(name, value)| (name.to_ascii_lowercase(), value)).collect()
    }

    /// Claude's unified limits: every `anthropic-ratelimit-unified-{window}-utilization`
    /// with its `-reset`.
    #[must_use]
    pub fn anthropic<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> PlanUsage {
        const PREFIX: &str = "anthropic-ratelimit-unified-";
        let headers = lower(headers);
        let get = |name: String| headers.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
        let mut windows: Vec<PlanWindow> = headers
            .iter()
            .filter_map(|(name, value)| {
                let window = name.strip_prefix(PREFIX)?.strip_suffix("-utilization").filter(|w| !w.is_empty())?;
                Some(PlanWindow {
                    name: window.into(),
                    label: window_label(window),
                    used: fraction(value)?,
                    resets_at: get(format!("{PREFIX}{window}-reset")).and_then(timestamp),
                })
            })
            .collect();
        sort(&mut windows);
        PlanUsage { windows }
    }

    /// The Codex backend's windows: `x-codex-{window}-used-percent` with its
    /// `-window-minutes` and `-reset-at` (or `-reset-after-seconds` from `now`).
    #[must_use]
    pub fn codex<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>, now: i64) -> PlanUsage {
        let headers = lower(headers);
        let get = |window: &str, field: &str| headers.iter().find(|(n, _)| *n == format!("x-codex-{window}-{field}")).map(|(_, v)| v.trim());
        let mut windows: Vec<PlanWindow> = headers
            .iter()
            .filter_map(|(name, value)| {
                let window = name.strip_prefix("x-codex-")?.strip_suffix("-used-percent").filter(|w| !w.is_empty())?;
                let minutes = get(window, "window-minutes").and_then(|m| m.parse::<u64>().ok()).filter(|m| *m > 0);
                let resets_at = get(window, "reset-at")
                    .and_then(timestamp)
                    .or_else(|| get(window, "reset-after-seconds").and_then(|s| s.parse::<i64>().ok()).map(|s| now + s));
                Some(PlanWindow {
                    name: window.into(),
                    label: minutes.map_or_else(|| capitalize(window), label),
                    used: percent(value.trim().parse().ok()?)?,
                    resets_at,
                })
            })
            .collect();
        windows.sort_by(|a, b| a.name.cmp(&b.name));
        PlanUsage { windows }
    }

    /// A Codex `codex.rate_limits` event: `rate_limits.{primary,secondary}`
    /// with `used_percent`, `window_minutes` and `reset_at` (or `reset_after_seconds`).
    #[must_use]
    pub fn codex_event(event: &Value, now: i64) -> Option<PlanUsage> {
        let mut windows: Vec<PlanWindow> = event
            .get("rate_limits")?
            .as_object()?
            .iter()
            .filter_map(|(name, w)| {
                let minutes = w["window_minutes"].as_u64().filter(|m| *m > 0);
                let at = |key: &str| w[key].as_i64().or_else(|| w[key].as_str().and_then(timestamp));
                let after = |key: &str| w[key].as_i64().map(|s| now + s);
                Some(PlanWindow {
                    name: name.clone(),
                    label: minutes.map_or_else(|| capitalize(name), label),
                    used: percent(w["used_percent"].as_f64()?)?,
                    resets_at: at("reset_at")
                        .or_else(|| at("resets_at"))
                        .or_else(|| after("reset_after_seconds"))
                        .or_else(|| after("resets_in_seconds")),
                })
            })
            .collect();
        windows.sort_by(|a, b| a.name.cmp(&b.name));
        (!windows.is_empty()).then_some(PlanUsage { windows })
    }

    /// Both header families from one response; `None` when it has neither.
    #[must_use]
    pub fn from_headers<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)> + Clone, now: i64) -> Option<PlanUsage> {
        let mut usage = anthropic(headers.clone());
        usage.windows.extend(codex(headers, now).windows);
        (!usage.is_empty()).then_some(usage)
    }

    /// [`from_headers`] of an HTTP response; `None` unless it answered a subscription credential.
    #[must_use]
    pub fn from_response(response: &reqwest::Response, subscription: bool) -> Option<PlanUsage> {
        if !subscription {
            return None;
        }
        let pairs: Vec<(&str, &str)> = response.headers().iter().filter_map(|(n, v)| Some((n.as_str(), v.to_str().ok()?))).collect();
        from_headers(pairs, unix_now())
    }

    /// `stream` without [`StreamEvent::PlanUsage`] unless it answers a subscription credential.
    pub fn subscription_only(subscription: bool, stream: EventStream) -> EventStream {
        use futures::StreamExt;
        if subscription { stream } else { Box::pin(stream.filter(|e| std::future::ready(!matches!(e, Ok(StreamEvent::PlanUsage(_)))))) }
    }

    /// `stream`, after a [`StreamEvent::PlanUsage`] when there is one.
    pub fn prepend(plan: Option<PlanUsage>, stream: EventStream) -> EventStream {
        use futures::StreamExt;
        match plan {
            Some(plan) => Box::pin(futures::stream::iter([Ok(StreamEvent::PlanUsage(plan))]).chain(stream)),
            None => stream,
        }
    }
}
