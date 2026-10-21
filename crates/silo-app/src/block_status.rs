//! What a block's pograms report (OSC 7501 records, OSC 9;4 progress),
//! reduced to the marks the pane header, tabs and switcher show. Program
//! text is untrusted: it is cleaned again here (an older or hostile server
//! may not have) and always shown under a trusted label.

use silo_proto::{
    model::{BlockStatus, ProgramStatus, ProgressState, StatusKind, StatusState},
    text::{MAX_APP, MAX_MESSAGE, MAX_TITLE, untrusted},
};

/// The strongest state of a block (or of several, for a tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Waiting for the user: "needs you".
    Blocked(Option<StatusKind>),
    Error,
    /// With a percent when a record reports one.
    Working(Option<u8>),
    Done,
}

impl Mark {
    /// blocked > error > working > done.
    fn rank(self) -> u8 {
        match self {
            Self::Blocked(_) => 3,
            Self::Error => 2,
            Self::Working(_) => 1,
            Self::Done => 0,
        }
    }

    fn of(record: &ProgramStatus) -> Option<Self> {
        Some(match record.state {
            StatusState::Blocked => Self::Blocked(record.kind),
            StatusState::Error => Self::Error,
            StatusState::Working => Self::Working(record.progress.map(|p| p.min(100))),
            StatusState::Done => Self::Done,
            StatusState::Idle | StatusState::Unknown => return None,
        })
    }

    /// The icon of a blocked record's kind.
    pub fn kind_icon(kind: Option<StatusKind>) -> Option<&'static str> {
        match kind? {
            StatusKind::Permission => Some("lock"),
            StatusKind::Question => Some("question"),
            StatusKind::Auth => Some("key"),
            StatusKind::Unknown => None,
        }
    }

    /// Words for the mark, for tooltips and the accessibility of tabs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Blocked(_) => "Needs you",
            Self::Error => "Error",
            Self::Working(_) => "Working",
            Self::Done => "Done",
        }
    }
}

/// The strongest of `marks`.
pub fn strongest(marks: impl IntoIterator<Item = Mark>) -> Option<Mark> {
    marks.into_iter().fold(None, |best: Option<Mark>, m| match best {
        Some(b) if b.rank() > m.rank() => Some(b),
        // Two working records: the one with a percent, else the lower one.
        Some(Mark::Working(Some(a))) if matches!(m, Mark::Working(None)) => Some(Mark::Working(Some(a))),
        Some(Mark::Working(Some(a))) if matches!(m, Mark::Working(Some(b)) if b > a) => Some(Mark::Working(Some(a))),
        _ => Some(m),
    })
}

/// A block's mark: its strongest record.
pub fn mark(status: &BlockStatus) -> Option<Mark> {
    strongest(status.records.iter().filter_map(Mark::of))
}

/// Whether any record is in `state`.
pub fn has(status: &BlockStatus, state: StatusState) -> bool {
    status.records.iter().any(|r| r.state == state)
}

/// OSC 9;4 progress as a bar: the filled fraction (`None`: indeterminate)
/// and whether it reports an error or a pause.
pub fn progress_bar(status: &BlockStatus) -> Option<(Option<f32>, ProgressState)> {
    let progress = status.progress?;
    let fraction = match progress.state {
        ProgressState::Indeterminate => None,
        ProgressState::Set | ProgressState::Error | ProgressState::Pause => Some(f32::from(progress.percent.unwrap_or(0).min(100)) / 100.0),
        ProgressState::Unknown => return None,
    };
    Some((fraction, progress.state))
}

/// Each record as `app · title · message`, cleaned, for a hover under the
/// trusted label.
pub fn record_lines(status: &BlockStatus) -> Vec<String> {
    status
        .records
        .iter()
        .filter(|r| Mark::of(r).is_some())
        .map(|r| {
            let parts = [untrusted(&r.app, MAX_APP), untrusted(&r.title, MAX_TITLE), untrusted(&r.message, MAX_MESSAGE)];
            let text = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ");
            let state = Mark::of(r).map_or("", Mark::label);
            if text.is_empty() { state.to_owned() } else { format!("{state}: {text}") }
        })
        .collect()
}

/// The trusted label program text is shown under: `host · session · block`.
pub fn trusted_label(host: &str, session: &str, block: &str) -> String {
    [host, session, block].iter().map(|s| untrusted(s, MAX_TITLE)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
}

/// The kind colours of marks on a surface `bg`.
fn colors(bg: u32) -> (u32, u32) {
    (crate::status::StatusKind::Success.color_on(bg), crate::status::StatusKind::Error.color_on(bg))
}

/// A mark as a small glyph on `bg` (`accent`: the surface's accent):
/// blocked is a dot, working a spinner, done a check, error a red mark.
pub(crate) fn glyph(mark: Mark, key: &str, size: f32, bg: u32, accent: u32, cx: &gpui::App) -> gpui::AnyElement {
    use gpui::{IntoElement as _, Styled as _, div, px, rgb};
    let (success, danger) = colors(bg);
    match mark {
        Mark::Blocked(_) => div().flex_none().size(px(size * 0.6)).rounded_full().bg(rgb(accent)).into_any_element(),
        Mark::Error => crate::assets::icon("alert", size, danger).into_any_element(),
        Mark::Working(_) => crate::browser_pane::spinner(key, size, theme_muted(bg), cx),
        Mark::Done => crate::assets::icon("check", size, success).into_any_element(),
    }
}

fn theme_muted(bg: u32) -> u32 {
    crate::theme::icon_muted_on(bg)
}

impl crate::workspace::Workspace {
    /// The pane header's status: "needs you" with the kind's icon, a
    /// spinner with its percent, a check or a red mark. Hovering lists the
    /// records under the block's trusted label.
    pub(crate) fn render_status_mark(&self, id: &str, palette: crate::adaptive::Chrome, cx: &gpui::App) -> Option<gpui::AnyElement> {
        use gpui::{InteractiveElement as _, IntoElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, px, rgb};
        let status = self.block_status(id)?;
        let mark = mark(status)?;
        let tip = std::iter::once(self.status_label(id, cx)).chain(record_lines(status)).collect::<Vec<_>>().join("\n");
        let bg = palette.surface;
        let (_, danger) = colors(bg);
        let mut el = div()
            .id(gpui::ElementId::Name(format!("pane-status-{id}").into()))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .text_size(px(11.0))
            .tooltip(crate::tooltip::Tooltip::text(tip))
            .child(glyph(mark, &format!("pane-{id}"), 12.0, bg, palette.accent, cx));
        el = match mark {
            Mark::Blocked(kind) => el
                .children(Mark::kind_icon(kind).map(|icon| crate::assets::icon(icon, 11.0, palette.accent)))
                .child(div().text_color(rgb(palette.accent)).child("needs you")),
            Mark::Working(Some(percent)) => el.child(div().text_color(rgb(palette.muted)).child(format!("{percent}%"))),
            Mark::Error => el.child(div().text_color(rgb(danger)).child("error")),
            Mark::Working(None) | Mark::Done => el,
        };
        Some(el.into_any_element())
    }

    /// OSC 9;4 progress as a thin bar under the pane header.
    pub(crate) fn render_progress_bar(&self, id: &str, palette: crate::adaptive::Chrome) -> Option<gpui::AnyElement> {
        use gpui::{IntoElement as _, ParentElement as _, Styled as _, div, px, relative, rgb};
        let (fraction, state) = progress_bar(self.block_status(id)?)?;
        let (_, danger) = colors(palette.surface);
        let color = match state {
            ProgressState::Error => danger,
            ProgressState::Pause => palette.muted,
            _ => palette.accent,
        };
        let fill = div().h_full().bg(rgb(color));
        let fill = match fraction {
            Some(f) => fill.w(relative(f)),
            // Indeterminate: a short segment, at rest.
            None => fill.w(relative(0.3)).opacity(0.6),
        };
        Some(div().absolute().bottom_0().left_0().right_0().h(px(2.0)).child(fill).into_any_element())
    }

    /// A tab's mark: the strongest state across its blocks.
    pub(crate) fn window_mark(&self, w: &silo_proto::model::WindowInfo) -> Option<Mark> {
        strongest(crate::floats::window_blocks(w).iter().filter_map(|b| mark(self.block_status(b)?)))
    }
}

/// A session's counts (`AttentionCounts`) as small badges: needs you,
/// errors, working, done; zeros left out.
pub(crate) fn count_badges(counts: silo_proto::model::AttentionCounts, bg: u32) -> Vec<gpui::AnyElement> {
    use gpui::{IntoElement as _, ParentElement as _, Styled as _, div, px, rgb};
    let (success, danger) = colors(bg);
    [
        (counts.blocked, crate::theme::accent_on(bg), "needs you"),
        (counts.error, danger, "error"),
        (counts.working, theme_muted(bg), "working"),
        (counts.done, success, "done"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, color, _)| {
        div()
            .flex_none()
            .h(px(16.0))
            .min_w(px(16.0))
            .px(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgb(color).opacity(0.18))
            .text_color(rgb(color))
            .text_size(px(10.5))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(n.to_string())
            .into_any_element()
    })
    .collect()
}

/// What the badges say, for a tooltip.
pub(crate) fn counts_text(counts: silo_proto::model::AttentionCounts) -> String {
    [(counts.blocked, "need you"), (counts.error, "failed"), (counts.working, "working"), (counts.done, "done")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_proto::model::Progress;

    fn record(id: &str, state: StatusState) -> ProgramStatus {
        ProgramStatus {
            id: id.into(),
            state,
            kind: None,
            progress: None,
            app: "claude".into(),
            title: String::new(),
            message: String::new(),
            updated_at: 0,
        }
    }

    fn status(records: Vec<ProgramStatus>) -> BlockStatus {
        BlockStatus { block_id: "b".into(), records, progress: None, busy: None }
    }

    #[test]
    fn the_strongest_record_wins() {
        let done = record("a", StatusState::Done);
        let working = ProgramStatus { progress: Some(40), ..record("b", StatusState::Working) };
        let blocked = ProgramStatus { kind: Some(StatusKind::Question), ..record("c", StatusState::Blocked) };
        let error = record("d", StatusState::Error);
        assert_eq!(mark(&status(vec![])), None);
        assert_eq!(mark(&status(vec![record("", StatusState::Idle)])), None, "idle shows nothing");
        assert_eq!(mark(&status(vec![done.clone()])), Some(Mark::Done));
        assert_eq!(mark(&status(vec![done.clone(), working.clone()])), Some(Mark::Working(Some(40))));
        assert_eq!(mark(&status(vec![working.clone(), error.clone(), done.clone()])), Some(Mark::Error));
        assert_eq!(mark(&status(vec![error, blocked, working, done])), Some(Mark::Blocked(Some(StatusKind::Question))));
        // Tabs: the strongest across blocks.
        assert_eq!(strongest([Mark::Done, Mark::Working(None)]), Some(Mark::Working(None)));
        assert_eq!(strongest([Mark::Working(Some(10)), Mark::Working(None)]), Some(Mark::Working(Some(10))));
        assert_eq!(strongest([Mark::Error, Mark::Blocked(None), Mark::Done]), Some(Mark::Blocked(None)));
    }

    #[test]
    fn states_are_found() {
        assert!(!has(&status(vec![record("", StatusState::Working)]), StatusState::Done));
        assert!(has(&status(vec![record("x", StatusState::Error)]), StatusState::Error));
    }

    #[test]
    fn program_text_is_cleaned_and_labelled() {
        let r = ProgramStatus { title: "evil\u{202e}txt.exe".into(), message: "line\none\u{200b}".into(), ..record("", StatusState::Blocked) };
        assert_eq!(record_lines(&status(vec![r])), ["Needs you: claude · eviltxt.exe · line one"]);
        assert_eq!(trusted_label("This Mac", "work", "api\u{2066}"), "This Mac · work · api");
        assert_eq!(trusted_label("This Mac", "work", ""), "This Mac · work");
    }

    #[test]
    fn progress_shows_as_a_bar() {
        let mut s = status(vec![]);
        assert_eq!(progress_bar(&s), None);
        s.progress = Some(Progress { state: ProgressState::Set, percent: Some(250) });
        assert_eq!(progress_bar(&s), Some((Some(1.0), ProgressState::Set)));
        s.progress = Some(Progress { state: ProgressState::Indeterminate, percent: None });
        assert_eq!(progress_bar(&s), Some((None, ProgressState::Indeterminate)));
    }
}
