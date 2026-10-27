//! Usage totals with API-equivalent costs, computed when read: each stored step's tokens priced at its own
//! model's list price (`otto_llm::catalog::list_pricing`, the catalog as otto loaded it, with the user's
//! refreshed cache) and its own prompt size (long-context tiers). The stored, billed `cost_usd` is kept as it
//! was recorded (0 on a subscription); steps recorded before otto knew API-equivalent costs are priced too.

use otto_llm::{Pricing, Usage};
use otto_proto::{UsageDay, UsageSummary, UsageTotals};
use otto_store::{UsageRecord, UsageRow};
use std::collections::HashMap;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// How many days `usage.summary` lists a day each.
pub(crate) const SUMMARY_DAYS: i64 = 30;

/// List prices looked up once per provider and model.
#[derive(Default)]
pub(crate) struct Prices(HashMap<(String, String), Option<Pricing>>);

impl Prices {
    /// `usage` at `provider`'s `model` list price; `None` when the catalog has none.
    pub(crate) fn api_cost(&mut self, provider: &str, model: &str, usage: &Usage) -> Option<f64> {
        let pricing = self.0.entry((provider.to_owned(), model.to_owned())).or_insert_with(|| otto_llm::catalog::list_pricing(provider, model));
        pricing.as_ref().map(|p| p.cost(usage))
    }
    fn add(&mut self, totals: &mut UsageTotals, record: &UsageRecord) {
        let api = self.api_cost(&record.provider, &record.model, &record.usage);
        totals.add_step(&record.provider, &record.model, &record.usage, record.cost_usd, api);
    }
}

/// The steps' totals, by model.
pub(crate) fn totals<'a>(prices: &mut Prices, records: impl IntoIterator<Item = &'a UsageRecord>) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for record in records {
        prices.add(&mut totals, record);
    }
    totals.sort_models();
    totals
}

/// Each turn's totals, by turn.
pub(crate) fn by_turn(prices: &mut Prices, rows: &[UsageRow]) -> HashMap<i64, UsageTotals> {
    let mut turns: HashMap<i64, UsageTotals> = HashMap::new();
    for row in rows {
        prices.add(turns.entry(row.turn).or_default(), &row.record);
    }
    turns.values_mut().for_each(UsageTotals::sort_models);
    turns
}

/// Where `usage.summary` reads from: the earlier of `since_ms` and the first of the days it lists.
#[must_use]
pub(crate) fn summary_from(since_ms: i64, utc_offset_minutes: i32, now_ms: i64) -> i64 {
    since_ms.min(day_start(now_ms, utc_offset_minutes) - (SUMMARY_DAYS - 1) * DAY_MS)
}

/// `usage.summary` from the rows since [`summary_from`]: the totals of those since `since_ms`, and the last
/// [`SUMMARY_DAYS`] days (to `now_ms`'s, days at `utc_offset_minutes`) that had usage.
#[must_use]
pub(crate) fn summary(rows: &[UsageRow], since_ms: i64, utc_offset_minutes: i32, now_ms: i64) -> UsageSummary {
    let mut prices = Prices::default();
    let first_day = day_start(now_ms, utc_offset_minutes) - (SUMMARY_DAYS - 1) * DAY_MS;
    let mut totals = UsageTotals::default();
    let mut sessions = std::collections::HashSet::new();
    let mut days: std::collections::BTreeMap<i64, UsageTotals> = std::collections::BTreeMap::new();
    for row in rows {
        let api = prices.api_cost(&row.record.provider, &row.record.model, &row.record.usage);
        let r = &row.record;
        if row.at >= since_ms {
            totals.add_step(&r.provider, &r.model, &r.usage, r.cost_usd, api);
            sessions.insert(row.session_id.as_str());
        }
        let day = day_start(row.at, utc_offset_minutes);
        if day >= first_day {
            let entry = days.entry(day).or_default();
            entry.add_step(&r.provider, &r.model, &r.usage, r.cost_usd, api);
        }
    }
    totals.sort_models();
    UsageSummary {
        since_ms,
        totals,
        sessions: u32::try_from(sessions.len()).unwrap_or(u32::MAX),
        days: days
            .into_iter()
            .map(|(start_ms, t)| UsageDay { start_ms, usage: t.usage, cost_usd: t.cost_usd, api_cost_usd: t.api_cost_usd })
            .collect(),
    }
}

/// The start of `at`'s day (ms) where days start at midnight `utc_offset_minutes` east of UTC.
fn day_start(at: i64, utc_offset_minutes: i32) -> i64 {
    let offset = i64::from(utc_offset_minutes) * 60_000;
    (at + offset).div_euclid(DAY_MS) * DAY_MS - offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, turn: i64, at: i64, provider: &str, model: &str, usage: Usage, cost_usd: f64) -> UsageRow {
        UsageRow {
            session_id: session.into(),
            turn,
            step: 1,
            at,
            record: UsageRecord { provider: provider.into(), model: model.into(), usage, cost_usd },
        }
    }
    fn tokens(input: u64, output: u64) -> Usage {
        Usage { input, output, ..Usage::default() }
    }

    #[test]
    fn days_start_at_the_clients_midnight() {
        // 2027-01-01T00:00Z is 1_798_761_600_000.
        let midnight = 1_798_761_600_000;
        assert_eq!(day_start(midnight + 1, 0), midnight);
        assert_eq!(day_start(midnight - 1, 0), midnight - DAY_MS);
        // India (+5:30): 2027-01-01T20:00Z is already the 2nd there, from 18:30Z.
        assert_eq!(day_start(midnight + 20 * 3_600_000, 330), midnight + DAY_MS - 330 * 60_000);
        assert_eq!(day_start(midnight + 3_600_000, -300), midnight - DAY_MS + 300 * 60_000);
    }

    #[test]
    fn a_summary_prices_each_step_and_lists_its_days() {
        let midnight = 1_798_761_600_000;
        let now = midnight + 12 * 3_600_000;
        let model = otto_llm::catalog::Catalog::global().models("anthropic", None).into_iter().find(|e| e.list_pricing.is_some()).unwrap();
        let id = model.info.id.clone();
        let price = model.list_pricing.unwrap();
        let rows = vec![
            row("old", 1, midnight - 40 * DAY_MS, "anthropic", &id, tokens(1_000, 100), 0.0),
            row("a", 1, midnight - 2 * DAY_MS, "anthropic", &id, tokens(10_000, 1_000), 0.0),
            row("a", 2, midnight + 1_000, "anthropic", &id, tokens(20_000, 2_000), 0.5),
            row("b", 1, midnight + 2_000, "ollama", "llama", tokens(5_000, 500), 0.0),
        ];
        assert!(summary_from(midnight, 0, now) <= midnight - 29 * DAY_MS);
        let today = summary(&rows, midnight, 0, now);
        assert_eq!(today.sessions, 2);
        assert_eq!(today.totals.usage, Usage { input: 25_000, output: 2_500, ..Usage::default() });
        assert!((today.totals.cost_usd - 0.5).abs() < 1e-9, "billed as stored");
        let api = price.cost(&tokens(20_000, 2_000));
        assert!((today.totals.api_cost_usd.unwrap() - api).abs() < 1e-12, "the unpriced model adds nothing");
        assert_eq!(today.totals.by_model.len(), 2);
        assert_eq!(today.totals.by_model[0].model, id, "priced first");
        assert_eq!(today.totals.by_model[1].api_cost_usd, None, "unknown, never $0");
        assert_eq!(today.totals.by_model[1].steps, 1);
        // Days: the last 30 (the one 40 days back left out), each a sum.
        assert_eq!(today.days.iter().map(|d| d.start_ms).collect::<Vec<_>>(), [midnight - 2 * DAY_MS, midnight]);
        let week = summary(&rows, midnight - 6 * DAY_MS, 0, now);
        assert_eq!(week.sessions, 2);
        assert_eq!(week.totals.by_model[0].steps, 2);
        assert!(summary(&[], midnight, 0, now).totals.api_cost_usd.is_none());
    }
}
