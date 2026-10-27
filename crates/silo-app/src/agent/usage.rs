//! The usage card (click the footer's token count or plan ring): what is left
//! of the provider's limits, how full the context is and the session's tokens;
//! and each reply's tokens, right of its copy/retry/branch row.

use chrono::{DateTime, TimeZone};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, relative, rgb};
use otto_llm::{PlanUsage, Usage};
use otto_proto::{ContextUse, TurnUsage, UsageTotals};

use super::{
    AgentPane, plan, ring,
    rows::menu,
    view::{grouped, provider_name},
};
use crate::{theme, tooltip::Tooltip};

/// A count in a few characters: `999`, `1.2k`, `12.4k`, `124k`, `1.2M`, `12M`.
pub(super) fn compact(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let (k, m) = (n as f64 / 1000.0, n as f64 / 1_000_000.0);
    if n < 1000 {
        n.to_string()
    } else if k < 99.95 {
        format!("{k:.1}k").replace(".0k", "k")
    } else if k < 999.5 {
        format!("{k:.0}k")
    } else if m < 9.95 {
        format!("{m:.1}M").replace(".0M", "M")
    } else {
        format!("{m:.0}M")
    }
}

/// A share as a whole percent (`89%`); a share above 0 never reads as 0 (`<1%`), nor one below 1 as 100.
pub(super) fn percent(share: f64) -> String {
    let pct = (share * 100.0).clamp(0.0, 100.0);
    if pct > 0.0 && pct < 0.5 {
        "<1%".into()
    } else if (99.5..100.0).contains(&pct) {
        ">99%".into()
    } else {
        format!("{pct:.0}%")
    }
}

/// The share of the prompt served from the provider's cache: `cache_read` of every input token (uncached,
/// read and written), as otto counts it (`Usage::cache_hit_rate`); `None` without input.
pub(super) fn hit_rate(usage: &Usage) -> Option<f64> {
    usage.cache_hit_rate()
}

/// A reply's line: `12.4k in · 1.2k out · 89% cached` (input counts cached tokens; no `cached` without a hit).
/// `None` when the turn reported no tokens.
pub(super) fn turn_label(usage: &Usage) -> Option<String> {
    let input = usage.total_input();
    if input == 0 && usage.output == 0 {
        return None;
    }
    let mut label = format!("{} in · {} out", compact(input), compact(usage.output));
    if let Some(rate) = hit_rate(usage).filter(|_| usage.cache_read > 0) {
        label.push_str(&format!(" · {} cached", percent(rate)));
    }
    Some(label)
}

/// A reply's tooltip: the turn's full breakdown, a line each.
pub(super) fn turn_tip(turn: &TurnUsage) -> String {
    let lines: Vec<String> = token_rows(&turn.usage, turn.cost_usd).into_iter().map(|(label, value)| format!("{label}: {value}")).collect();
    lines.join("\n")
}

/// The breakdown of `usage` (a turn's or the session's) as label/value rows: every input kind, output (with
/// its reasoning when reported), the total, the cache hit rate when there was input, the cost when known.
pub(super) fn token_rows(usage: &Usage, cost_usd: f64) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("Input", grouped(usage.input)),
        ("Cache read (hit)", grouped(usage.cache_read)),
        ("Cache write", grouped(usage.cache_write)),
        ("Output", grouped(usage.output)),
    ];
    if usage.reasoning > 0 {
        rows.push(("Reasoning (in output)", grouped(usage.reasoning)));
    }
    rows.push(("Total", grouped(usage.total_input() + usage.output)));
    if let Some(rate) = hit_rate(usage) {
        rows.push(("Cache hit rate", percent(rate)));
    }
    if cost_usd > 0.0 {
        rows.push(("Cost", cost(cost_usd)));
    }
    rows
}

/// `$0.31`; under a cent `<$0.01`.
fn cost(usd: f64) -> String {
    if usd < 0.005 { "<$0.01".into() } else { format!("${usd:.2}") }
}

/// The context line: `48,213 / 200,000` and `24%`; `None` without a known window.
pub(super) fn context_row(context: ContextUse) -> Option<(String, String)> {
    let share = ring::fraction(context.used, context.window)?;
    Some((format!("{} / {}", grouped(context.used), grouped(context.window)), percent(f64::from(share))))
}

/// A limit window as shown: its label, share used (0–1), `38% left` and when it resets.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Limit {
    pub label: String,
    pub used: f32,
    pub left: String,
    pub reset: Option<String>,
}

/// Each window of `usage` (shortest first), as of `now`.
pub(super) fn limits<Tz: TimeZone>(usage: &PlanUsage, now: &DateTime<Tz>) -> Vec<Limit>
where
    Tz::Offset: std::fmt::Display,
{
    usage
        .windows
        .iter()
        .map(|w| {
            let used = plan::used(w, now.timestamp());
            Limit {
                label: w.label.clone(),
                used,
                left: format!("{} left", percent(f64::from(1.0 - used))),
                reset: w.resets_at.and_then(|at| plan::resets(at, now)),
            }
        })
        .collect()
}

/// Why no limits show: a subscription reports them with each reply; API keys and local models never do.
pub(super) fn limits_note(provider: &str, subscription: bool) -> String {
    if subscription { "Shows after the next reply".into() } else { format!("Not reported by {}", provider_name(provider)) }
}

/// A label on the left, its value on the right (tabular figures).
fn stat(label: impl Into<SharedString>, value: impl Into<SharedString>, color: u32) -> gpui::Div {
    div()
        .h(px(22.0))
        .px(px(menu::ROW_X))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .text_size(px(12.5))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(theme::muted())).child(label.into()))
        .child(div().flex_none().whitespace_nowrap().font_features(tabular()).text_color(rgb(color)).child(value.into()))
}

fn tabular() -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// A thin bar at `fill` (0–1) in `color`.
fn bar(fill: f32, color: u32) -> gpui::Div {
    div()
        .mx(px(menu::ROW_X))
        .mb(px(4.0))
        .h(px(4.0))
        .rounded_full()
        .bg(rgb(theme::mix(theme::background(), theme::foreground(), 0.12)))
        .child(div().h_full().w(relative(fill.clamp(0.0, 1.0))).rounded_full().bg(rgb(color)))
}

/// A muted line where a section has nothing to show.
fn note(text: impl Into<SharedString>) -> gpui::Div {
    div().h(px(22.0)).px(px(menu::ROW_X)).flex().items_center().text_size(px(12.0)).text_color(rgb(theme::muted())).child(text.into())
}

fn heading(label: &'static str) -> gpui::Div {
    div()
        .h(px(22.0))
        .px(px(menu::ROW_X))
        .flex()
        .items_center()
        .text_size(px(11.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::muted()))
        .child(label)
}

impl AgentPane {
    /// The usage card's body: Limits, Context, then the session's tokens.
    pub(super) fn render_usage(&mut self, provider: Option<&str>) -> AnyElement {
        let fg = theme::foreground();
        let mut card = div().id("agent-usage").flex().flex_col().child(heading("Limits"));
        let subscription = provider.and_then(|p| self.catalog.as_ref().and_then(|c| c.subscription(p)));
        let shown = provider.and_then(|p| self.plan_of(p)).map(|u| limits(u, &chrono::Local::now())).unwrap_or_default();
        if shown.is_empty() {
            card = card.child(note(provider.map_or_else(|| "No model".into(), |p| limits_note(p, subscription.is_some()))));
        }
        for limit in shown {
            let level = ring::Fill::of(limit.used);
            let detail = match &limit.reset {
                Some(reset) => format!("{} · {reset}", limit.left),
                None => limit.left.clone(),
            };
            card = card
                .child(stat(format!("{} · {} used", limit.label, percent(f64::from(limit.used))), detail, fg))
                .child(bar(limit.used, level.color()));
        }
        if let Some(plan) = provider.and_then(|p| plan::plan_name(p, subscription.as_deref())) {
            card = card.child(note(plan));
        }
        card = card.child(menu::divider()).child(heading("Context"));
        let Some(m) = self.mirror() else {
            return card.child(note("No session yet")).into_any_element();
        };
        card = match context_row(m.info.context) {
            Some((used, pct)) => {
                let share = ring::fraction(m.info.context.used, m.info.context.window).unwrap_or_default();
                card.child(stat(used, pct, ring::Fill::of(share).color())).child(bar(share, ring::Fill::of(share).color()))
            }
            None => card.child(note("The model's window is unknown")),
        };
        let UsageTotals { usage, cost_usd, .. } = &m.info.usage;
        card = card.child(menu::divider()).child(heading("Session tokens"));
        for (label, value) in token_rows(usage, *cost_usd) {
            card = card.child(stat(label, value, fg));
        }
        card.into_any_element()
    }

    /// The reply `item`'s tokens, right of its action row: muted, the full breakdown on hover; none without data.
    pub(super) fn render_turn_usage(&self, item: &str, key: &str) -> Option<AnyElement> {
        let turn = self.mirror()?.turn_usage_of(item)?;
        let label = turn_label(&turn.usage)?;
        Some(
            div()
                .id(SharedString::from(format!("turn-usage-{key}")))
                .ml_auto()
                .flex_none()
                .h(px(24.0))
                .px(px(4.0))
                .flex()
                .items_center()
                .whitespace_nowrap()
                .text_size(px(11.5))
                .font_features(tabular())
                .text_color(rgb(theme::muted()))
                .tooltip(Tooltip::text(turn_tip(turn)))
                .child(label)
                .into_any_element(),
        )
    }

    /// The card's provider: the session's, else the model the composer would use.
    pub(super) fn usage_provider(&self) -> Option<String> {
        self.mirror().map(|m| m.info.provider.clone()).filter(|p| !p.is_empty()).or_else(|| self.model().map(|(p, _)| p))
    }

    /// Open (or close) the usage card from the footer.
    pub(super) fn toggle_usage(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.open_overlay(super::Overlay::Usage, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use otto_llm::PlanWindow;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, reasoning: u64) -> Usage {
        Usage { input, output, cache_read, cache_write, reasoning }
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1000), "1k");
        assert_eq!(compact(1_234), "1.2k");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(99_949), "99.9k");
        assert_eq!(compact(99_950), "100k");
        assert_eq!(compact(124_000), "124k");
        assert_eq!(compact(999_499), "999k");
        assert_eq!(compact(999_500), "1M", "never 1000k");
        assert_eq!(compact(1_250_000), "1.2M");
        assert_eq!(compact(12_400_000), "12M");
    }

    #[test]
    fn percents_never_round_to_the_ends() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.004), "<1%");
        assert_eq!(percent(0.891), "89%");
        assert_eq!(percent(0.996), ">99%");
        assert_eq!(percent(1.0), "100%");
        assert_eq!(percent(1.7), "100%", "clamped");
    }

    #[test]
    fn cache_hit_rate_counts_every_input_token() {
        assert_eq!(hit_rate(&Usage::default()), None, "no input: no rate");
        assert_eq!(hit_rate(&usage(10, 5, 90, 0, 0)), Some(0.9));
        // Tokens written to the cache were not served from it.
        assert_eq!(hit_rate(&usage(10, 5, 60, 30, 0)), Some(0.6));
        assert_eq!(hit_rate(&usage(10, 5, 0, 0, 0)), Some(0.0));
    }

    #[test]
    fn a_replys_line_and_tooltip() {
        assert_eq!(turn_label(&usage(1_200, 1_234, 11_200, 0, 0)).as_deref(), Some("12.4k in · 1.2k out · 90% cached"));
        assert_eq!(turn_label(&usage(900, 40, 0, 0, 0)).as_deref(), Some("900 in · 40 out"), "no hit: no cached");
        assert_eq!(turn_label(&Usage::default()), None, "nothing reported: nothing shown");
        let turn =
            TurnUsage { turn: 3, item_id: Some("r".into()), usage: usage(1_200, 1_234, 11_200, 100, 300), cost_usd: 0.0412, ..TurnUsage::default() };
        assert_eq!(
            turn_tip(&turn),
            "Input: 1,200\nCache read (hit): 11,200\nCache write: 100\nOutput: 1,234\nReasoning (in output): 300\nTotal: 13,734\nCache hit rate: 90%\nCost: $0.04"
        );
        // No reasoning, rate or cost when there is none.
        let quiet = TurnUsage { usage: usage(0, 12, 0, 0, 0), ..TurnUsage::default() };
        assert_eq!(turn_tip(&quiet), "Input: 0\nCache read (hit): 0\nCache write: 0\nOutput: 12\nTotal: 12");
        assert_eq!(cost(0.001), "<$0.01");
    }

    #[test]
    fn context_and_limits() {
        assert_eq!(context_row(ContextUse { used: 48_213, window: 200_000 }), Some(("48,213 / 200,000".into(), "24%".into())));
        assert_eq!(context_row(ContextUse { used: 10, window: 0 }), None, "unknown window");
        const NOW: i64 = 1_799_323_200;
        let now = FixedOffset::east_opt(0).unwrap().timestamp_opt(NOW, 0).unwrap();
        let plan = PlanUsage {
            windows: vec![
                PlanWindow { name: "5h".into(), label: "5-hour".into(), used: 0.62, resets_at: Some(NOW + 6000) },
                PlanWindow { name: "7d".into(), label: "Weekly".into(), used: 0.9, resets_at: Some(NOW - 1) },
            ],
        };
        let shown = limits(&plan, &now);
        assert_eq!(shown[0], Limit { label: "5-hour".into(), used: 0.62, left: "38% left".into(), reset: Some("resets in 1h 40m".into()) });
        assert_eq!(shown[1], Limit { label: "Weekly".into(), used: 0.0, left: "100% left".into(), reset: None }, "past its reset: empty");
        assert_eq!(limits_note("anthropic", false), "Not reported by Anthropic");
        assert_eq!(limits_note("openai", true), "Shows after the next reply");
    }
}
