//! The plan usage ring after the footer's count: how much of the
//! subscription's limits (Claude's 5-hour and weekly windows, ChatGPT's) is
//! used, from otto's `plan.usage` and its live `plan.usage` events.

use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone};
use gpui::{AnyElement, Context, prelude::*, px};
use otto_llm::PlanUsage;
use otto_proto::{Event, Method, ProviderPlan, ServerMessage};

use super::{AgentPane, ring, view::Fit};
use crate::{theme::Continuous as _, tooltip::Tooltip};

/// A window's share as shown: 0 once its reset has passed (the provider reports the new one with the next reply).
pub(super) fn used(window: &otto_llm::PlanWindow, now: i64) -> f32 {
    if window.resets_at.is_some_and(|at| at <= now) { 0.0 } else { window.used.clamp(0.0, 1.0) }
}

/// The ring's fill: the tightest window's share; `None` without windows.
pub(super) fn tightest(usage: &PlanUsage, now: i64) -> Option<f32> {
    usage.windows.iter().map(|w| used(w, now)).max_by(f32::total_cmp)
}

/// `1h 40m`, `40m` (rounded up to the minute).
fn until(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{}m", m.max(1)),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// When a window resets: relative under a day (`resets in 1h 40m`), else its day and time (`resets Mon 09:00`).
pub(super) fn resets<Tz: TimeZone>(at: i64, now: &DateTime<Tz>) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let left = at - now.timestamp();
    if left <= 0 {
        return None;
    }
    if left < 86_400 {
        return Some(format!("resets in {}", until(left)));
    }
    let local = now.timezone().timestamp_opt(at, 0).single()?;
    Some(format!("resets {}", local.format("%a %H:%M")))
}

/// The provider's subscription as people name it (`Claude Max`); `None` for an unknown plan.
pub(super) fn plan_name(provider: &str, plan: Option<&str>) -> Option<String> {
    let plan = plan.filter(|p| !p.trim().is_empty() && *p != "sub")?;
    let brand = match provider {
        "anthropic" => "Claude",
        "openai" => "ChatGPT",
        other => other,
    };
    Some(format!("{brand} {plan}"))
}

/// The ring's tooltip: a line per window (`5-hour: 62% · resets in 1h 40m`), then the plan's name.
pub(super) fn tip<Tz: TimeZone>(usage: &PlanUsage, provider: &str, plan: Option<&str>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut lines: Vec<String> = usage
        .windows
        .iter()
        .map(|w| {
            let pct = (used(w, now.timestamp()) * 100.0).round();
            match w.resets_at.and_then(|at| resets(at, now)) {
                Some(reset) => format!("{}: {pct:.0}% · {reset}", w.label),
                None => format!("{}: {pct:.0}%", w.label),
            }
        })
        .collect();
    lines.extend(plan_name(provider, plan));
    lines.join("\n")
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl AgentPane {
    /// `provider`'s usage as last reported: otto's live events and `plan.usage`, else the session's snapshot.
    pub(super) fn plan_of(&self, provider: &str) -> Option<&PlanUsage> {
        self.plans
            .get(provider)
            .or_else(|| self.mirror().filter(|m| m.info.provider == provider).and_then(|m| m.info.plan.as_ref()))
            .filter(|u| !u.is_empty())
    }

    /// What otto knows of every provider's limits, before the first reply in this pane.
    pub(super) fn load_plans(&mut self, cx: &mut Context<Self>) {
        self.run(
            cx,
            |c| async move { c.call_typed::<Vec<ProviderPlan>>(Method::PlanUsage { provider: None }).await },
            |this, result, _| {
                // An older otto has no `plan.usage`: no ring until it reports.
                for plan in result.unwrap_or_default() {
                    this.plans.insert(plan.provider, plan.usage);
                }
            },
        );
    }

    /// Follow otto's `plan.usage` events (any session's reply reports its provider's limits), and
    /// `auth.changed` (a sign-in made elsewhere, e.g. the CLI): every view of this otto reloads.
    pub(super) fn watch_plans(&mut self, cx: &mut Context<Self>) {
        if self.plan_watch.is_some() {
            return;
        }
        // `None`: the sign-ins changed.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<ProviderPlan>>();
        let target = self.target.clone();
        self.rt.spawn(async move {
            loop {
                if let Ok(connection) = super::client::connection(&target).await {
                    let mut events = connection.events();
                    loop {
                        match events.recv().await {
                            Ok(Ok(ServerMessage::Event { session_id: None, event, .. })) => {
                                let sent = match *event {
                                    Event::PlanUsage { provider, usage } => tx.send(Some(ProviderPlan { provider, usage })),
                                    Event::AuthChanged {} => tx.send(None),
                                    _ => Ok(()),
                                };
                                if sent.is_err() {
                                    return;
                                }
                            }
                            Ok(Ok(_)) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Ok(Err(_)) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        self.plan_watch = Some(cx.spawn(async move |this, cx| {
            while let Some(plan) = rx.recv().await {
                let applied = this.update(cx, |this, cx| match plan {
                    Some(plan) => {
                        this.plans.insert(plan.provider, plan.usage);
                        cx.notify();
                    }
                    None => {
                        if cx.default_global::<super::client::AuthSeen>().fresh(&this.target, Instant::now()) {
                            cx.set_global(super::client::OttoChanged(this.target.clone()));
                        }
                    }
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    /// The ring after the token count, before the buttons: `provider`'s subscription usage, filled to its tightest
    /// window. None for API keys and local models, while nothing is known, or when too narrow. Opens the usage card.
    pub(super) fn render_plan(&self, fit: Fit, provider: Option<&str>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let provider = provider.filter(|_| fit.ring)?;
        let plan = self.catalog.as_ref()?.subscription(provider)?;
        let fill = tightest(self.plan_of(provider)?, unix_now())?;
        let pane = cx.entity().downgrade();
        let owner = provider.to_owned();
        let tip = Tooltip::live(move |cx| {
            let tip = pane.upgrade().and_then(|p| p.read(cx).plan_of(&owner).map(|u| tip(u, &owner, Some(&plan), &chrono::Local::now())));
            tip.unwrap_or_default().into()
        });
        Some(
            gpui::div()
                .id("agent-plan")
                .flex_none()
                .h(px(22.0))
                .px(px(4.0))
                .flex()
                .items_center()
                .continuous(6.0)
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::hover_fill()))
                .when(self.overlay == Some(super::Overlay::Usage), |el| el.bg(crate::theme::hover_fill()))
                .tooltip(tip)
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| this.toggle_usage(window, cx)))
                .child(ring::ring(fill, ring::Fill::of(fill).color()))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use otto_llm::PlanWindow;

    fn window(name: &str, label: &str, used: f32, resets_at: Option<i64>) -> PlanWindow {
        PlanWindow { name: name.into(), label: label.into(), used, resets_at }
    }

    /// Thu 2027-01-07 12:00 UTC.
    const NOW: i64 = 1_799_323_200;

    #[test]
    fn the_ring_fills_to_the_tightest_window() {
        let usage = PlanUsage { windows: vec![window("5h", "5-hour", 0.62, None), window("7d", "Weekly", 0.91, None)] };
        assert_eq!(tightest(&usage, NOW), Some(0.91));
        assert_eq!(ring::Fill::of(0.91), ring::Fill::Warn);
        assert_eq!(tightest(&PlanUsage::default(), NOW), None, "nothing known: no ring");
        // A window whose reset has passed counts as empty until the next report.
        let usage = PlanUsage { windows: vec![window("5h", "5-hour", 0.99, Some(NOW - 1)), window("7d", "Weekly", 0.2, Some(NOW + 10))] };
        assert_eq!(tightest(&usage, NOW), Some(0.2));
    }

    #[test]
    fn tooltips_say_when_each_window_resets() {
        let utc = FixedOffset::east_opt(0).unwrap().timestamp_opt(NOW, 0).unwrap();
        let usage = PlanUsage {
            windows: vec![
                window("5h", "5-hour", 0.62, Some(NOW + 6000)),
                window("7d", "Weekly", 0.18, Some(NOW + 4 * 86_400 - 3 * 3600)),
                window("7d_opus", "Weekly Opus", 0.05, None),
            ],
        };
        assert_eq!(
            tip(&usage, "anthropic", Some("Max"), &utc),
            "5-hour: 62% · resets in 1h 40m\nWeekly: 18% · resets Mon 09:00\nWeekly Opus: 5%\nClaude Max"
        );
        // Day and time in the viewer's zone.
        let tokyo = FixedOffset::east_opt(9 * 3600).unwrap().timestamp_opt(NOW, 0).unwrap();
        assert!(tip(&usage, "anthropic", None, &tokyo).contains("Weekly: 18% · resets Mon 18:00"));
        // Under a day stays relative; the plan's name only when known.
        let usage =
            PlanUsage { windows: vec![window("primary", "5-hour", 0.5, Some(NOW + 86_399)), window("secondary", "Weekly", 1.0, Some(NOW + 30))] };
        assert_eq!(tip(&usage, "openai", Some("sub"), &utc), "5-hour: 50% · resets in 24h\nWeekly: 100% · resets in 1m");
        assert_eq!(tip(&usage, "openai", Some("Plus"), &utc).lines().last(), Some("ChatGPT Plus"));
        // Past its reset: empty, no reset time.
        let usage = PlanUsage { windows: vec![window("5h", "5-hour", 0.9, Some(NOW - 5))] };
        assert_eq!(tip(&usage, "anthropic", None, &utc), "5-hour: 0%");
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(until(6000), "1h 40m");
        assert_eq!(until(3600), "1h");
        assert_eq!(until(59), "1m");
        assert_eq!(until(2400), "40m");
        assert_eq!(until(0), "1m");
    }
}
