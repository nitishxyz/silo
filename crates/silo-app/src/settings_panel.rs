//! Snapshot-driven settings UI. The owner applies events and refreshes the snapshot;
//! rendering never reads files, changes preferences, or controls the daemon.
//!
//! Layout follows a system-settings pattern: a sidebar of icon tiles on the
//! left, the selected page on the right made of grouped, aligned rows.

use gpui::{
    Anchor, AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, FontWeight, IntoElement, Render,
    ScrollHandle, Subscription, Window, anchored, deferred, div, prelude::*, px, rgb,
};

pub use crate::hosts::Density;
pub use crate::terminal_prefs::ColorSlot;
pub use silo_proto::model::CursorShape;
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    controls::{Segmented, Toggle},
    terminal_prefs::{self, TerminalPrefs},
    text_field::{FieldEvent, TextField},
    theme,
};

#[derive(Clone, Debug)]
pub struct SettingsHost {
    pub id: String,
    pub name: String,
    pub address: String,
    pub status: String,
    pub removable: bool,
}

/// Populate from the command registry, not a separate hard-coded shortcut table.
#[derive(Clone, Debug)]
pub struct SettingsShortcut {
    pub label: String,
    pub shortcut: String,
}

#[derive(Clone, Debug, Default)]
pub struct SettingsServer {
    pub status: String,
    pub version: Option<String>,
    /// A server log exists and can be opened.
    pub has_log: bool,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    /// The server's iroh EndpointId, once connected.
    pub endpoint_id: Option<String>,
    /// Why remote access is unavailable (no paths), if it is.
    pub remote_error: Option<String>,
    pub devices: Vec<SettingsDevice>,
    /// A fresh pairing ticket to show (and copy, or scan).
    pub invite: Option<String>,
}

/// A device paired with the local server.
#[derive(Clone, Debug)]
pub struct SettingsDevice {
    pub id: String,
    pub name: String,
    /// e.g. "Last seen 5 min ago".
    pub seen: String,
}

/// A bundled theme, for the picker and its preview tiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeChoice {
    pub id: String,
    pub name: String,
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub ansi: [u32; 16],
}

/// Silo's own terminal preferences (from `terminal.json`) and their effect.
#[derive(Clone, Debug)]
pub struct TerminalSettings {
    /// Selected theme id.
    pub theme: String,
    pub themes: Vec<ThemeChoice>,
    /// The preferred family (empty = Automatic) and the one rendering.
    pub font_family: String,
    pub font_in_use: String,
    /// Installed monospaced families for the picker, supported Nerd Fonts
    /// first.
    pub font_choices: Vec<String>,
    /// What Automatic resolves to, and whether that is a Nerd Font (false:
    /// none installed, using the fallback).
    pub font_automatic: String,
    pub font_automatic_nerd: bool,
    /// The preferred family is not installed.
    pub font_missing: bool,
    pub font_size: f32,
    /// Effective color per slot and whether it overrides the theme.
    pub colors: Vec<(ColorSlot, u32, bool)>,
    pub palette: [u32; 16],
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub scrollback_lines: u64,
    /// Problems loading the file or the font, shown on the page.
    pub problems: Vec<String>,
}

impl TerminalSettings {
    #[must_use]
    pub fn from_prefs(prefs: &TerminalPrefs, font_in_use: String) -> Self {
        let effective = prefs.effective();
        let c = effective.config;
        let colors = ColorSlot::ALL
            .into_iter()
            .map(|slot| {
                let value = match slot {
                    ColorSlot::Foreground => c.foreground,
                    ColorSlot::Background => c.background,
                    ColorSlot::Cursor => c.cursor,
                    ColorSlot::Selection => effective.selection,
                };
                (slot, value, prefs.colors.get(slot).is_some())
            })
            .collect();
        Self {
            theme: prefs.theme.clone(),
            themes: terminal_prefs::THEMES
                .iter()
                .map(|p| ThemeChoice {
                    id: p.id.into(),
                    name: p.name.into(),
                    background: p.background,
                    foreground: p.foreground,
                    cursor: p.cursor,
                    ansi: p.ansi,
                })
                .collect(),
            font_family: prefs.font_family.clone(),
            font_automatic: font_in_use.clone(),
            font_in_use,
            font_choices: Vec::new(),
            font_automatic_nerd: false,
            font_missing: false,
            font_size: prefs.font_size,
            colors,
            palette: c.palette,
            cursor_shape: prefs.cursor_shape,
            cursor_blink: prefs.cursor_blink,
            scrollback_lines: prefs.scrollback_lines,
            problems: Vec::new(),
        }
    }

    fn color(&self, slot: ColorSlot) -> (u32, bool) {
        self.colors.iter().find(|(s, _, _)| *s == slot).map_or((0, false), |(_, c, o)| (*c, *o))
    }

    /// Picker rows for `query`: `None` is Automatic, then matching families.
    fn font_entries(&self, query: &str) -> Vec<Option<String>> {
        let query = query.trim().to_lowercase();
        let automatic = query.is_empty() || "automatic default".contains(&query) || self.font_automatic.to_lowercase().contains(&query);
        font_entries(&self.font_choices, automatic, &query)
    }

    /// Whether `entry` is the stored preference.
    fn font_selected(&self, entry: Option<&str>) -> bool {
        font_selected(&self.font_family, entry)
    }

    /// Short state of the font preference shown under the Family row.
    fn font_status(&self) -> Option<String> {
        if self.font_missing {
            Some(format!("Not installed; using {}", self.font_in_use))
        } else if self.font_family.is_empty() && !self.font_automatic_nerd {
            Some(format!("No Nerd Font installed; using {}", self.font_in_use))
        } else if self.font_family.is_empty() {
            Some(format!("Using {}", self.font_in_use))
        } else {
            None
        }
    }
}

/// The UI (chrome) font preference from `app.json` and its effect; the
/// terminal font is separate ([`TerminalSettings`]).
#[derive(Clone, Debug, Default)]
pub struct UiFontSettings {
    /// The preferred family; empty = Automatic (the system font).
    pub family: String,
    /// Installed text families for the picker, alphabetically.
    pub choices: Vec<String>,
    /// The preferred family is not installed.
    pub missing: bool,
}

impl UiFontSettings {
    fn font_entries(&self, query: &str) -> Vec<Option<String>> {
        let query = query.trim().to_lowercase();
        let automatic = query.is_empty() || "automatic default".contains(&query) || "system font".contains(&query);
        font_entries(&self.choices, automatic, &query)
    }

    fn font_selected(&self, entry: Option<&str>) -> bool {
        font_selected(&self.family, entry)
    }

    /// Short state shown under the UI font row.
    fn font_status(&self) -> Option<String> {
        if self.missing {
            Some("Not installed; using the system font".into())
        } else if self.family.is_empty() {
            Some("System font".into())
        } else {
            None
        }
    }
}

/// Picker rows over `choices` for a lowercased `query`: `None` (Automatic)
/// first when it matches, then matching families.
fn font_entries(choices: &[String], automatic: bool, query: &str) -> Vec<Option<String>> {
    let mut out = Vec::new();
    if automatic {
        out.push(None);
    }
    out.extend(choices.iter().filter(|f| query.is_empty() || f.to_lowercase().contains(query)).cloned().map(Some));
    out
}

/// Whether a picker `entry` is the stored `preference` (empty = Automatic).
fn font_selected(preference: &str, entry: Option<&str>) -> bool {
    match entry {
        None => preference.is_empty(),
        Some(family) => crate::fonts::key(family) == crate::fonts::key(preference),
    }
}

/// Which preference a font picker edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontTarget {
    Terminal,
    Ui,
}

#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
    pub density: Density,
    pub show_pane_headers: bool,
    pub ui_font: UiFontSettings,
    pub terminal: TerminalSettings,
    pub hosts: Vec<SettingsHost>,
    pub shortcuts: Vec<SettingsShortcut>,
    pub server: SettingsServer,
    pub version: String,
    /// Short form shown in the row; `revision_full` is copied.
    pub revision: String,
    pub revision_full: String,
    /// An operation result supplied by the owner; never an optimistic success message.
    pub message: Option<String>,
}

impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self {
            density: Density::Comfortable,
            show_pane_headers: true,
            ui_font: UiFontSettings::default(),
            terminal: TerminalSettings::from_prefs(&TerminalPrefs::default(), terminal_prefs::DEFAULT_FONT.into()),
            hosts: Vec::new(),
            shortcuts: Vec::new(),
            server: SettingsServer::default(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: "Not supplied".into(),
            revision_full: "Not supplied".into(),
            message: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerAction {
    Start,
    Stop,
    Restart,
}

#[derive(Clone, Debug)]
pub enum SettingsEvent {
    Close,
    SetDensity(Density),
    SetPaneHeaders(bool),
    /// The UI font family; empty = Automatic (the system font).
    SetUiFontFamily(String),
    SetTheme(String),
    SetFontFamily(String),
    SetFontSize(f32),
    /// `None` clears the override (back to the theme's color).
    SetColor(ColorSlot, Option<u32>),
    SetCursorShape(CursorShape),
    SetCursorBlink(bool),
    SetScrollback(u64),
    ResetTerminal,
    /// Re-read `terminal.json` (after editing it by hand).
    ReloadTerminal,
    /// Open `terminal.json` in a text editor.
    OpenConfig,
    /// Show the settings file in Finder.
    RevealConfig,
    AddHost,
    RenameHost(String),
    RemoveHost(String),
    OpenServerLog,
    Server(ServerAction),
    /// `pair.create`: show a new pairing ticket.
    PairDevice,
    RevokeDevice(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Appearance,
    Terminal,
    Keyboard,
    Hosts,
    Server,
    About,
}

impl Section {
    const ALL: [Self; 6] = [Self::Appearance, Self::Terminal, Self::Keyboard, Self::Hosts, Self::Server, Self::About];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Keyboard => "Keyboard",
            Self::Hosts => "Hosts",
            Self::Server => "Server",
            Self::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Appearance => "settings-shapes",
            Self::Terminal => "terminal",
            Self::Keyboard => "settings-keyboard",
            Self::Hosts => "globe",
            Self::Server => "settings-hard-drive",
            Self::About => "settings-info",
        }
    }

    /// Tile color behind the sidebar icon.
    fn tint(self) -> u32 {
        match self {
            Self::Appearance => 0x008b_5cf6,
            Self::Terminal => 0x0014_b8a6,
            Self::Keyboard => 0x0063_66f1,
            Self::Hosts => 0x000e_a5e9,
            Self::Server => 0x00f5_9e0b,
            Self::About => 0x0000_78d4,
        }
    }

    fn keywords(self) -> &'static str {
        match self {
            Self::Appearance => {
                "general preferences density compact comfortable pane headers layout theme ui interface font family typeface system automatic"
            }
            Self::Terminal => {
                "theme colors palette preview font family nerd automatic size foreground background cursor selection shape block bar underline blink blinking scrollback history lines reset defaults settings file json configuration config edit open reload finder"
            }
            Self::Keyboard => "shortcuts keys commands bindings",
            Self::Hosts => "remote local address add remove iroh",
            Self::Server => "daemon status version log start stop restart sessions",
            Self::About => "silo version revision build libghostty",
        }
    }
}

pub struct SettingsPanel {
    snapshot: SettingsSnapshot,
    selected: Section,
    search: Entity<TextField>,
    query: String,
    pending_server: Option<ServerAction>,
    pending_remove: Option<String>,
    scroll: ScrollHandle,
    /// The open font picker (terminal or UI font), its search field and
    /// highlighted row; one at a time.
    font_menu: Option<FontTarget>,
    font_search: Entity<TextField>,
    font_query: String,
    font_highlight: usize,
    font_scroll: ScrollHandle,
    color_fields: Vec<(ColorSlot, Entity<TextField>)>,
    scrollback_field: Entity<TextField>,
    /// Input that could not be applied (bad hex, number out of range).
    field_error: Option<String>,
    /// The open theme dropdown (by trigger id) and its highlighted row.
    theme_menu: Option<&'static str>,
    theme_highlight: usize,
    /// Panel-level focus for keyboard navigation of the theme dropdown.
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsPanel {}

/// Panel widths below this collapse the sidebar to icon-only width.
const NARROW: f32 = 620.0;
const ROW_MIN_H: f32 = 44.0;
/// Corner radius of the hosting card (see `settings.rs`).
pub const CARD_RADIUS: f32 = 12.0;
/// Height of the details toolbar (page title, close button).
const TOOLBAR_H: f32 = 48.0;

impl SettingsPanel {
    pub fn new(snapshot: SettingsSnapshot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextField::new("Search", cx));
        let mut subscriptions = vec![cx.subscribe(&search, |this, _, event, cx| {
            match event {
                FieldEvent::Changed(query) => {
                    this.query = query.trim().to_lowercase();
                    this.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                }
                FieldEvent::Cancel => cx.emit(SettingsEvent::Close),
                _ => {}
            }
            cx.notify();
        })];
        let font_search = cx.new(|cx| TextField::new("Search fonts", cx));
        subscriptions.push(cx.subscribe_in(&font_search, window, |this, _, event, window, cx| {
            this.font_search_event(event, window, cx);
        }));
        let color_fields: Vec<_> = ColorSlot::ALL
            .into_iter()
            .map(|slot| {
                let field = cx.new(|cx| TextField::new("#rrggbb", cx));
                subscriptions.push(cx.subscribe(&field, move |this, field, event, cx| match event {
                    FieldEvent::Submit => {
                        let text = field.read(cx).text.clone();
                        match terminal_prefs::parse_hex(&text) {
                            Some(color) => {
                                this.field_error = None;
                                cx.emit(SettingsEvent::SetColor(slot, Some(color)));
                            }
                            None => {
                                this.field_error = Some(format!("{} must be a hex color like #1e1e2e.", slot.label()));
                                cx.notify();
                            }
                        }
                    }
                    FieldEvent::Cancel => this.sync_fields(cx),
                    _ => {}
                }));
                (slot, field)
            })
            .collect();
        let scrollback_field = cx.new(|cx| TextField::new("10000", cx));
        subscriptions.push(cx.subscribe(&scrollback_field, |this, field, event, cx| match event {
            FieldEvent::Submit => {
                let text: String = field.read(cx).text.chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
                let range = silo_proto::model::TerminalConfig::MIN_SCROLLBACK..=silo_proto::model::TerminalConfig::MAX_SCROLLBACK;
                match text.parse::<u64>() {
                    Ok(lines) if range.contains(&lines) => {
                        this.field_error = None;
                        cx.emit(SettingsEvent::SetScrollback(lines));
                    }
                    _ => {
                        this.field_error = Some(format!("Scrollback must be a number of lines from {} to {}.", range.start(), range.end()));
                        cx.notify();
                    }
                }
            }
            FieldEvent::Cancel => this.sync_fields(cx),
            _ => {}
        }));
        let mut this = Self {
            snapshot,
            selected: Section::Appearance,
            search,
            query: String::new(),
            pending_server: None,
            pending_remove: None,
            scroll: ScrollHandle::new(),
            font_menu: None,
            font_search,
            font_query: String::new(),
            font_highlight: 0,
            font_scroll: ScrollHandle::new(),
            color_fields,
            scrollback_field,
            field_error: None,
            theme_menu: None,
            theme_highlight: 0,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.sync_fields(cx);
        this
    }

    pub fn refresh(&mut self, snapshot: SettingsSnapshot, cx: &mut Context<Self>) {
        let old = &self.snapshot.terminal;
        let new = &snapshot.terminal;
        let fields_changed = old.colors != new.colors || old.scrollback_lines != new.scrollback_lines;
        self.snapshot = snapshot;
        self.pending_server = None;
        self.pending_remove = None;
        if fields_changed {
            self.sync_fields(cx);
        }
        cx.notify();
    }

    /// Show the snapshot's values in the text fields (dropping edits).
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let t = &self.snapshot.terminal;
        self.field_error = None;
        for (slot, field) in &self.color_fields {
            let text = terminal_prefs::hex_string(t.color(*slot).0);
            field.update(cx, |f, cx| f.replace_text(text, cx));
        }
        let lines = t.scrollback_lines.to_string();
        self.scrollback_field.update(cx, |f, cx| f.replace_text(lines, cx));
        cx.notify();
    }

    /// Whether the Server page (with paired devices) is on screen.
    #[must_use]
    pub fn showing_server(&self) -> bool {
        self.selected == Section::Server || (!self.query.is_empty() && self.matches(Section::Server))
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.search.read(cx).focus.clone();
        focus.focus(window, cx);
    }

    fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        self.selected = section;
        self.theme_menu = None;
        self.font_menu = None;
        self.pending_server = None;
        self.pending_remove = None;
        self.query.clear();
        self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.search.update(cx, |field, cx| field.replace_text("", cx));
        cx.notify();
    }

    fn words(&self) -> impl Iterator<Item = &str> {
        self.query.split_whitespace()
    }

    fn text_matches(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.words().all(|word| text.contains(word))
    }

    fn matches(&self, section: Section) -> bool {
        let mut text = format!("{} {}", section.label(), section.keywords());
        if matches!(section, Section::Terminal | Section::Appearance) {
            for choice in &self.snapshot.terminal.themes {
                text.push_str(&format!(" {}", choice.name));
            }
        }
        if section == Section::Keyboard {
            for shortcut in &self.snapshot.shortcuts {
                text.push_str(&format!(" {} {}", shortcut.label, shortcut.shortcut));
            }
        }
        if section == Section::Hosts {
            for host in &self.snapshot.hosts {
                text.push_str(&format!(" {} {}", host.name, host.address));
            }
        }
        self.text_matches(&text)
    }

    // ---- building blocks ----

    fn button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        kind: ButtonKind,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (bg, fg, hover) = match kind {
            ButtonKind::Secondary => (theme::tab_active(), theme::foreground(), theme::row_active()),
            ButtonKind::Danger => (theme::danger(), 0x00ff_ffff, theme::danger()),
        };
        div()
            .id(id)
            .flex_none()
            .h(px(26.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(13.0)
            .whitespace_nowrap()
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .bg(rgb(bg))
            .text_color(rgb(fg))
            .when(kind == ButtonKind::Secondary, |b| b.border_1().border_color(rgb(theme::border())))
            .cursor_pointer()
            .hover(move |s| if kind == ButtonKind::Secondary { s.bg(rgb(hover)) } else { s.opacity(0.88) })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .into_any_element()
    }

    fn emit_button(
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        event: SettingsEvent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Self::button(id, label, ButtonKind::Secondary, move |_, cx| cx.emit(event.clone()), cx)
    }

    /// One aligned preference row: title (and optional description) on the
    /// left, control on the right; stacks when the content is narrow.
    fn row(
        title: impl Into<gpui::SharedString>,
        description: Option<gpui::SharedString>,
        control: Option<AnyElement>,
        narrow: bool,
    ) -> AnyElement {
        div()
            .w_full()
            .min_h(px(ROW_MIN_H))
            .px(px(14.0))
            .py(px(10.0))
            .flex()
            .gap(px(12.0))
            .when(narrow, |r| r.flex_col().items_start().gap(px(8.0)))
            .when(!narrow, |r| r.items_center().justify_between())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child(title.into()))
                    .children(description.map(|d| {
                        div().text_size(px(11.5)).line_height(px(15.0)).text_color(rgb(theme::muted())).child(d)
                    })),
            )
            .children(control.map(|c| div().flex_none().max_w_full().flex().items_center().gap(px(6.0)).child(c)))
            .into_any_element()
    }

    /// A read-only value shown right-aligned in a row.
    fn value(text: impl Into<gpui::SharedString>) -> AnyElement {
        div()
            .min_w_0()
            .max_w(px(320.0))
            .text_ellipsis()
            .whitespace_nowrap()
            .overflow_hidden()
            .text_size(px(12.5))
            .text_color(rgb(theme::muted()))
            .child(text.into())
            .into_any_element()
    }

    fn group(title: Option<&'static str>, rows: Vec<AnyElement>) -> AnyElement {
        let count = rows.len();
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .continuous(10.0)
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(theme::popover()));
        for (index, row) in rows.into_iter().enumerate() {
            card = card.child(row);
            if index + 1 < count {
                card = card.child(div().mx(px(14.0)).h(px(1.0)).bg(rgb(theme::border())));
            }
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(title.map(|t| {
                div()
                    .px(px(4.0))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::muted()))
                    .child(t)
            }))
            .child(card)
            .into_any_element()
    }

    fn note(text: &'static str) -> AnyElement {
        div().px(px(4.0)).text_size(px(11.5)).line_height(px(16.0)).text_color(rgb(theme::muted())).child(text).into_any_element()
    }

    /// The kit-style animated switch; reports the requested state.
    fn switch(
        id: &'static str,
        label: &'static str,
        on: bool,
        event: fn(bool) -> SettingsEvent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity().downgrade();
        Toggle::new(id, label, on)
            .on_toggle(move |value, _, cx| {
                let _ = panel.update(cx, |_, cx| cx.emit(event(value)));
            })
            .into_any_element()
    }

    /// The kit-style segmented control with a sliding thumb.
    fn segmented<T: Copy + PartialEq + 'static>(
        id: &'static str,
        options: &'static [(T, &'static str)],
        current: T,
        segment_width: f32,
        event: fn(T) -> SettingsEvent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = options.iter().position(|(value, _)| *value == current).unwrap_or(0);
        let panel = cx.entity().downgrade();
        Segmented::new(id, options.iter().map(|(_, label)| *label), selected)
            .segment_width(segment_width)
            .on_select(move |index, _, cx| {
                if let Some((value, _)) = options.get(index) {
                    let value = *value;
                    let _ = panel.update(cx, |_, cx| cx.emit(event(value)));
                }
            })
            .into_any_element()
    }

    fn density_control(&self, cx: &mut Context<Self>) -> AnyElement {
        Self::segmented(
            "settings-density",
            &[(Density::Compact, "Compact"), (Density::Comfortable, "Comfortable")],
            self.snapshot.density,
            92.0,
            SettingsEvent::SetDensity,
            cx,
        )
    }

    /// A text field styled as an inline input.
    fn field_box(field: &Entity<TextField>, width: f32) -> AnyElement {
        div()
            .w(px(width))
            .h(px(26.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .continuous(6.0)
            .bg(rgb(theme::background()))
            .border_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(field.clone())
            .into_any_element()
    }

    fn swatch(color: u32, size: f32) -> AnyElement {
        div()
            .flex_none()
            .size(px(size))
            .continuous(5.0)
            .bg(rgb(color))
            .border_1()
            .border_color(rgb(theme::border()))
            .into_any_element()
    }

    /// A small rendering of the effective terminal colors in the chosen
    /// font, so changes are visible before looking at a pane.
    fn terminal_preview(&self) -> AnyElement {
        let t = &self.snapshot.terminal;
        let fg = t.color(ColorSlot::Foreground).0;
        let bg = t.color(ColorSlot::Background).0;
        let cursor = t.color(ColorSlot::Cursor).0;
        let selection = t.color(ColorSlot::Selection).0;
        let p = t.palette;
        let span = |text: &'static str, color: u32| div().text_color(rgb(color)).child(text);
        let line = || div().flex().whitespace_nowrap();
        let size = t.font_size.clamp(10.0, 15.0);
        let cursor_el = {
            let base = div().h(px(size * 1.25)).w(px(size * 0.6));
            match t.cursor_shape {
                CursorShape::Bar => base.w(px(2.0)).bg(rgb(cursor)),
                CursorShape::Underline => div().flex().items_end().h(px(size * 1.25)).child(div().h(px(2.0)).w(px(size * 0.6)).bg(rgb(cursor))),
                CursorShape::Hollow => base.border_1().border_color(rgb(cursor)),
                CursorShape::Block => base.bg(rgb(cursor)),
            }
        };
        let mut swatches = div().flex().gap(px(4.0)).pt(px(6.0));
        for color in p {
            swatches = swatches.child(div().size(px(10.0)).continuous(2.0).bg(rgb(color)));
        }
        div()
            .w_full()
            .p(px(14.0))
            .continuous(10.0)
            .border_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(bg))
            .text_color(rgb(fg))
            .font_family(t.font_in_use.clone())
            .text_size(px(size))
            .line_height(px(size * 1.35))
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(line().child(span("~/silo ", p[4])).child(span("main ", p[5])).child(span("$ ", fg)).child("ls --color"))
            .child(
                line()
                    .gap(px(size * 0.6))
                    .child(span("Cargo.toml", fg))
                    .child(span("crates/", p[12]))
                    .child(span("build.sh", p[2]))
                    .child(div().bg(rgb(selection)).text_color(rgb(fg)).child("notes.md")),
            )
            .child(line().child(span("error: ", p[1])).child(span("warning: ", p[3])).child(span("ok", p[10])))
            .child(line().items_center().child(span("~/silo ", p[4])).child(span("$ ", fg)).child(cursor_el))
            .child(swatches)
            .into_any_element()
    }

    /// Compact palette strip for a theme: background chip with a few ANSI
    /// accents.
    fn theme_swatch(choice: &ThemeChoice) -> AnyElement {
        let mut strip = div()
            .flex_none()
            .h(px(16.0))
            .px(px(3.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .continuous(4.0)
            .bg(rgb(choice.background))
            .border_1()
            .border_color(rgb(theme::border()));
        strip = strip.child(div().w(px(6.0)).h(px(8.0)).continuous(1.0).bg(rgb(choice.foreground)));
        for i in [1, 2, 4, 5] {
            strip = strip.child(div().size(px(6.0)).continuous(3.0).bg(rgb(choice.ansi[i])));
        }
        strip.into_any_element()
    }

    fn open_theme_menu(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme_menu == Some(id) {
            self.theme_menu = None;
        } else {
            let t = &self.snapshot.terminal;
            self.theme_highlight = t.themes.iter().position(|c| c.id == t.theme).unwrap_or(0);
            self.theme_menu = Some(id);
            // Arrow keys, Return and Escape go to the panel while open.
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn choose_theme(&mut self, index: usize, cx: &mut Context<Self>) {
        self.theme_menu = None;
        if let Some(choice) = self.snapshot.terminal.themes.get(index)
            && choice.id != self.snapshot.terminal.theme
        {
            cx.emit(SettingsEvent::SetTheme(choice.id.clone()));
        }
        cx.notify();
    }

    /// Keyboard navigation for the open theme dropdown; true if handled.
    fn theme_menu_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        if self.theme_menu.is_none() {
            return false;
        }
        let count = self.snapshot.terminal.themes.len().max(1);
        match key {
            "down" => self.theme_highlight = (self.theme_highlight + 1) % count,
            "up" => self.theme_highlight = (self.theme_highlight + count - 1) % count,
            "home" => self.theme_highlight = 0,
            "end" => self.theme_highlight = count - 1,
            "enter" | "space" => {
                let index = self.theme_highlight;
                self.choose_theme(index, cx);
                return true;
            }
            "escape" | "tab" => self.theme_menu = None,
            _ => return false,
        }
        cx.notify();
        true
    }

    /// The theme picker: a popup button showing the current theme, with a
    /// menu of built-in themes (swatch, name, check on the current one).
    fn theme_dropdown(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let t = &self.snapshot.terminal;
        let current = t.themes.iter().find(|c| c.id == t.theme);
        let open = self.theme_menu == Some(id);
        let trigger = div()
            .id(id)
            .h(px(28.0))
            .min_w(px(196.0))
            .pl(px(8.0))
            .pr(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .continuous(7.0)
            .bg(rgb(theme::tab_active()))
            .border_1()
            .border_color(rgb(if open { theme::accent() } else { theme::border() }))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::row_active())))
            .children(current.map(Self::theme_swatch))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(12.5))
                    .child(current.map_or_else(|| "Choose…".to_owned(), |c| c.name.clone())),
            )
            .child(icon("chevron", 11.0, theme::muted()))
            .on_click(cx.listener(move |this, _, window, cx| this.open_theme_menu(id, window, cx)));

        let mut root = div().relative().child(trigger);
        if open {
            let mut list = div()
                .id((id, 1_usize))
                .w(px(232.0))
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .continuous(9.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .font_family(theme::ui_font())
                .text_color(rgb(theme::foreground()))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.theme_menu = None;
                    cx.notify();
                }));
            for (index, choice) in t.themes.iter().enumerate() {
                let selected = choice.id == t.theme;
                let highlighted = index == self.theme_highlight;
                list = list.child(
                    div()
                        .id(("settings-theme-option", index))
                        .h(px(30.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .continuous(6.0)
                        .cursor_pointer()
                        .when(highlighted, |r| r.bg(rgb(theme::row_active())))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.theme_highlight != index {
                                this.theme_highlight = index;
                                cx.notify();
                            }
                        }))
                        .child(Self::theme_swatch(choice))
                        .child(div().flex_1().min_w_0().text_size(px(12.5)).child(choice.name.clone()))
                        .child(div().w(px(12.0)).children(selected.then(|| icon("check", 12.0, theme::accent()))))
                        .on_click(cx.listener(move |this, _, _, cx| this.choose_theme(index, cx))),
                );
            }
            // Escape the card's clipping: draw above everything, anchored
            // under the trigger's right edge, kept inside the window.
            root = root.child(
                div().absolute().top(px(32.0)).right_0().child(deferred(
                    anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.0)).child(list),
                ).with_priority(1)),
            );
        }
        root.into_any_element()
    }

    fn font_entries(&self, target: FontTarget, query: &str) -> Vec<Option<String>> {
        match target {
            FontTarget::Terminal => self.snapshot.terminal.font_entries(query),
            FontTarget::Ui => self.snapshot.ui_font.font_entries(query),
        }
    }

    fn font_selected(&self, target: FontTarget, entry: Option<&str>) -> bool {
        match target {
            FontTarget::Terminal => self.snapshot.terminal.font_selected(entry),
            FontTarget::Ui => self.snapshot.ui_font.font_selected(entry),
        }
    }

    fn open_font_menu(&mut self, target: FontTarget, window: &mut Window, cx: &mut Context<Self>) {
        if self.font_menu == Some(target) {
            self.close_font_menu(window, cx);
            return;
        }
        self.theme_menu = None;
        self.font_menu = Some(target);
        self.font_query.clear();
        self.font_search.update(cx, |f, cx| f.replace_text("", cx));
        self.font_highlight =
            self.font_entries(target, "").iter().position(|e| self.font_selected(target, e.as_deref())).unwrap_or(0);
        self.font_scroll.scroll_to_item(self.font_highlight);
        let focus = self.font_search.read(cx).focus.clone();
        focus.focus(window, cx);
        cx.notify();
    }

    fn close_font_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.font_menu.take().is_some() {
            // Keep Escape and the theme menu keys working on the panel.
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    fn choose_font(&mut self, entry: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.font_menu else { return };
        self.close_font_menu(window, cx);
        if !self.font_selected(target, entry.as_deref()) {
            let family = entry.unwrap_or_default();
            cx.emit(match target {
                FontTarget::Terminal => SettingsEvent::SetFontFamily(family),
                FontTarget::Ui => SettingsEvent::SetUiFontFamily(family),
            });
        }
    }

    fn font_search_event(&mut self, event: &FieldEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.font_menu else { return };
        let entries = self.font_entries(target, &self.font_query);
        let count = entries.len();
        match event {
            FieldEvent::Changed(text) => {
                self.font_query = text.clone();
                self.font_highlight = 0;
                self.font_scroll.scroll_to_item(0);
            }
            FieldEvent::Down if count > 0 => {
                self.font_highlight = (self.font_highlight + 1) % count;
                self.font_scroll.scroll_to_item(self.font_highlight);
            }
            FieldEvent::Up if count > 0 => {
                self.font_highlight = (self.font_highlight + count - 1) % count;
                self.font_scroll.scroll_to_item(self.font_highlight);
            }
            FieldEvent::Submit => {
                if let Some(entry) = entries.get(self.font_highlight).cloned() {
                    self.choose_font(entry, window, cx);
                }
                return;
            }
            FieldEvent::Cancel => {
                self.close_font_menu(window, cx);
                return;
            }
            FieldEvent::Up | FieldEvent::Down => {}
        }
        cx.notify();
    }

    /// A font picker: a popup button with the current family; the menu has
    /// a search field and lists Automatic plus installed families (monospaced
    /// for the terminal, text faces for the UI), each in its own face.
    fn font_dropdown(&self, target: FontTarget, cx: &mut Context<Self>) -> AnyElement {
        let open = self.font_menu == Some(target);
        let (preference, missing) = match target {
            FontTarget::Terminal => (&self.snapshot.terminal.font_family, self.snapshot.terminal.font_missing),
            FontTarget::Ui => (&self.snapshot.ui_font.family, self.snapshot.ui_font.missing),
        };
        let label = if preference.is_empty() { "Automatic".to_owned() } else { preference.clone() };
        let border = if missing {
            theme::danger()
        } else if open {
            theme::accent()
        } else {
            theme::border()
        };
        let trigger = div()
            .id(match target {
                FontTarget::Terminal => "settings-font-family",
                FontTarget::Ui => "settings-ui-font-family",
            })
            .h(px(28.0))
            .min_w(px(196.0))
            .max_w(px(260.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .continuous(7.0)
            .bg(rgb(theme::tab_active()))
            .border_1()
            .border_color(rgb(border))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::row_active())))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(12.5))
                    .child(label),
            )
            .child(icon("chevron", 11.0, theme::muted()))
            .on_click(cx.listener(move |this, _, window, cx| this.open_font_menu(target, window, cx)));

        let mut root = div().relative().child(trigger);
        if open {
            let entries = self.font_entries(target, &self.font_query);
            let mut list = div()
                .id("settings-font-list")
                .max_h(px(264.0))
                .overflow_y_scroll()
                .track_scroll(&self.font_scroll)
                .p(px(4.0))
                .flex()
                .flex_col();
            if entries.is_empty() {
                list = list.child(div().px(px(8.0)).py(px(6.0)).text_size(px(12.0)).text_color(rgb(theme::muted())).child("No matching fonts"));
            }
            for (index, entry) in entries.into_iter().enumerate() {
                let selected = self.font_selected(target, entry.as_deref());
                let highlighted = index == self.font_highlight;
                let (name, detail, face): (String, Option<String>, gpui::SharedString) = match (&entry, target) {
                    (None, FontTarget::Terminal) => {
                        ("Automatic".into(), Some(self.snapshot.terminal.font_automatic.clone()), theme::ui_font())
                    }
                    (None, FontTarget::Ui) => ("Automatic".into(), Some("System font".into()), crate::fonts::UI_SYSTEM_FONT.into()),
                    (Some(family), FontTarget::Terminal) => {
                        (family.clone(), crate::fonts::is_nerd(family).then(|| "Nerd Font".into()), family.clone().into())
                    }
                    (Some(family), FontTarget::Ui) => (family.clone(), None, family.clone().into()),
                };
                list = list.child(
                    div()
                        .id(("settings-font-option", index))
                        .flex_none()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .continuous(6.0)
                        .cursor_pointer()
                        .when(highlighted, |r| r.bg(rgb(theme::row_active())))
                        .on_mouse_move(cx.listener(move |this, _, _, cx| {
                            if this.font_highlight != index {
                                this.font_highlight = index;
                                cx.notify();
                            }
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .font_family(face)
                                .text_size(px(12.5))
                                .child(name),
                        )
                        .children(detail.map(|d| {
                            div()
                                .flex_none()
                                .max_w(px(130.0))
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::muted()))
                                .child(d)
                        }))
                        .child(div().w(px(12.0)).flex_none().children(selected.then(|| icon("check", 12.0, theme::accent()))))
                        .on_click(cx.listener(move |this, _, window, cx| this.choose_font(entry.clone(), window, cx))),
                );
            }
            let menu = div()
                .id("settings-font-menu")
                .w(px(300.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .continuous(9.0)
                .bg(rgb(theme::popover()))
                .border_1()
                .border_color(rgb(theme::border()))
                .shadow_lg()
                .font_family(theme::ui_font())
                .text_color(rgb(theme::foreground()))
                .occlude()
                .on_mouse_down_out(cx.listener(|this, _, window, cx| this.close_font_menu(window, cx)))
                .child(
                    div()
                        .h(px(32.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .border_b_1()
                        .border_color(rgb(theme::border()))
                        .text_size(px(12.5))
                        .child(icon("settings-search", 12.0, theme::muted()))
                        .child(self.font_search.clone()),
                )
                .child(list);
            root = root.child(
                div().absolute().top(px(32.0)).right_0().child(
                    deferred(anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.0)).child(menu)).with_priority(1),
                ),
            );
        }
        root.into_any_element()
    }

    #[allow(clippy::too_many_lines)]
    fn terminal_page(&self, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = &self.snapshot.terminal;
        let mut out = Vec::new();
        let problems: Vec<String> = t.problems.iter().cloned().chain(self.field_error.clone()).collect();
        if !problems.is_empty() {
            let mut banner = div()
                .px(px(12.0))
                .py(px(8.0))
                .continuous(8.0)
                .border_1()
                .border_color(rgb(theme::danger()))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .text_size(px(12.0));
            for problem in problems {
                banner = banner.child(problem);
            }
            out.push(banner.into_any_element());
        }
        out.push(self.terminal_preview());

        out.push(Self::group(None, vec![Self::row("Theme", None, Some(self.theme_dropdown("settings-terminal-theme", cx)), false)]));

        let size = t.font_size;
        let step = |id: &'static str, label: &'static str, delta: f32, cx: &mut Context<Self>| {
            let next = (size + delta).clamp(terminal_prefs::MIN_FONT_SIZE, terminal_prefs::MAX_FONT_SIZE);
            Self::button(id, label, ButtonKind::Secondary, move |_, cx| cx.emit(SettingsEvent::SetFontSize(next)), cx)
        };
        let size_control = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(step("settings-font-smaller", "\u{2212}", -1.0, cx))
            .child(div().min_w(px(44.0)).flex().justify_center().text_size(px(12.5)).child(format!("{size} pt")))
            .child(step("settings-font-larger", "+", 1.0, cx))
            .into_any_element();
        let font_rows = vec![
            Self::row("Family", t.font_status().map(Into::into), Some(self.font_dropdown(FontTarget::Terminal, cx)), narrow),
            Self::row("Size", Some("This Mac only".into()), Some(size_control), narrow),
        ];
        out.push(Self::group(Some("Font"), font_rows));

        let mut color_rows = Vec::new();
        for (slot, field) in &self.color_fields {
            let (color, overridden) = t.color(*slot);
            let slot = *slot;
            let mut control = div().flex().items_center().gap(px(6.0)).child(Self::swatch(color, 20.0)).child(Self::field_box(field, 92.0));
            if overridden {
                control = control.child(Self::button(
                    ("settings-color-reset", slot as usize),
                    "Reset",
                    ButtonKind::Secondary,
                    move |_, cx| cx.emit(SettingsEvent::SetColor(slot, None)),
                    cx,
                ));
            }
            let description: Option<gpui::SharedString> = overridden.then(|| "Custom".into());
            color_rows.push(Self::row(slot.label(), description, Some(control.into_any_element()), narrow));
        }
        out.push(Self::group(Some("Colors"), color_rows));

        let blink = t.cursor_blink;
        out.push(Self::group(
            Some("Cursor"),
            vec![
                Self::row(
                    "Shape",
                    None,
                    Some(Self::segmented(
                        "settings-cursor-shape",
                        &[(CursorShape::Block, "Block"), (CursorShape::Bar, "Bar"), (CursorShape::Underline, "Underline")],
                        t.cursor_shape,
                        76.0,
                        SettingsEvent::SetCursorShape,
                        cx,
                    )),
                    narrow,
                ),
                Self::row("Blinking", None, Some(Self::switch("settings-cursor-blink", "Cursor blinking", blink, SettingsEvent::SetCursorBlink, cx)), false),
            ],
        ));

        out.push(Self::group(
            Some("Scrollback"),
            vec![Self::row(
                "History",
                None,
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(Self::field_box(&self.scrollback_field, 92.0))
                        .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child("lines"))
                        .into_any_element(),
                ),
                narrow,
            )],
        ));

        out.push(Self::group(
            None,
            vec![Self::row(
                "Restore defaults",
                None,
                Some(Self::emit_button("settings-terminal-reset", "Restore", SettingsEvent::ResetTerminal, cx)),
                false,
            )],
        ));

        out.push(Self::group(
            Some("Settings file"),
            vec![Self::row(
                "terminal.json",
                None,
                Some(
                    div()
                        .flex()
                        .gap(px(6.0))
                        .child(Self::emit_button("settings-terminal-reveal", "Show in Finder", SettingsEvent::RevealConfig, cx))
                        .child(Self::emit_button("settings-terminal-config", "Edit", SettingsEvent::OpenConfig, cx))
                        .child(Self::emit_button("settings-terminal-reload", "Reload", SettingsEvent::ReloadTerminal, cx))
                        .into_any_element(),
                ),
                narrow,
            )],
        ));
        out
    }

    // ---- pages ----

    #[allow(clippy::too_many_lines)]
    fn page(&self, section: Section, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let s = &self.snapshot;
        let mut out = Vec::new();
        match section {
            Section::Appearance => {
                out.push(Self::group(
                    Some("Layout"),
                    vec![
                        Self::row(
                            "Pane density",
                            None,
                            Some(self.density_control(cx)),
                            narrow,
                        ),
                        Self::row(
                            "Pane headers",
                            None,
                            Some(Self::switch(
                                "settings-headers",
                                "Pane headers",
                                s.show_pane_headers,
                                SettingsEvent::SetPaneHeaders,
                                cx,
                            )),
                            false,
                        ),
                    ],
                ));
                out.push(Self::group(
                    Some("Colors"),
                    vec![Self::row(
                        "Theme",
                        None,
                        Some(self.theme_dropdown("settings-appearance-theme", cx)),
                        false,
                    )],
                ));
                out.push(Self::group(
                    Some("Font"),
                    vec![Self::row(
                        "Interface font",
                        s.ui_font.font_status().map(Into::into),
                        Some(self.font_dropdown(FontTarget::Ui, cx)),
                        narrow,
                    )],
                ));
            }
            Section::Terminal => out.extend(self.terminal_page(narrow, cx)),
            Section::Keyboard => {
                let show_all = self.query.is_empty()
                    || self.text_matches(&format!("{} {}", section.label(), section.keywords()));
                let rows: Vec<AnyElement> = s
                    .shortcuts
                    .iter()
                    .filter(|k| show_all || self.text_matches(&format!("{} {}", k.label, k.shortcut)))
                    .map(|k| Self::row(k.label.clone(), None, Some(keycap(&k.shortcut)), false))
                    .collect();
                if rows.is_empty() {
                    out.push(Self::note("No shortcuts."));
                } else {
                    out.push(Self::group(None, rows));
                }
            }
            Section::Hosts => {
                let mut rows = Vec::new();
                for (index, host) in s.hosts.iter().enumerate() {
                    let confirming = self.pending_remove.as_deref() == Some(host.id.as_str());
                    let control = if confirming {
                        let id = host.id.clone();
                        Some(
                            div()
                                .flex()
                                .gap(px(6.0))
                                .child(Self::button(
                                    ("settings-remove-cancel", index),
                                    "Cancel",
                                    ButtonKind::Secondary,
                                    |this, cx| {
                                        this.pending_remove = None;
                                        cx.notify();
                                    },
                                    cx,
                                ))
                                .child(Self::button(
                                    ("settings-remove-confirm", index),
                                    "Remove",
                                    ButtonKind::Danger,
                                    move |this, cx| {
                                        this.pending_remove = None;
                                        cx.emit(SettingsEvent::RemoveHost(id.clone()));
                                    },
                                    cx,
                                ))
                                .into_any_element(),
                        )
                    } else if host.removable {
                        let id = host.id.clone();
                        Some(
                            div()
                                .flex()
                                .gap(px(6.0))
                                .child(Self::emit_button(("settings-rename-host", index), "Rename…", SettingsEvent::RenameHost(id.clone()), cx))
                                .child(Self::button(
                                    ("settings-remove-host", index),
                                    "Remove…",
                                    ButtonKind::Secondary,
                                    move |this, cx| {
                                        this.pending_remove = Some(id.clone());
                                        cx.notify();
                                    },
                                    cx,
                                ))
                                .into_any_element(),
                        )
                    } else {
                        None
                    };
                    // Remote addresses are long: a short, ellipsized form and
                    // a copy button; the full value stays one click away.
                    let control = if host.address.is_empty() || confirming {
                        control
                    } else {
                        let address = host.address.clone();
                        Some(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(div().max_w(px(140.0)).child(Self::value(host.address.clone())))
                                .child(Self::button(
                                    ("settings-copy-host", index),
                                    "Copy",
                                    ButtonKind::Secondary,
                                    move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(address.clone())),
                                    cx,
                                ))
                                .children(control)
                                .into_any_element(),
                        )
                    };
                    rows.push(Self::row(host.name.clone(), Some(host.status.clone().into()), control, narrow));
                }
                if !rows.is_empty() {
                    out.push(Self::group(None, rows));
                }
                out.push(Self::group(
                    None,
                    vec![Self::row(
                        "Remote host",
                        Some("Pair with a ticket from another Mac".into()),
                        Some(Self::emit_button("settings-add-host", "Add…", SettingsEvent::AddHost, cx)),
                        false,
                    )],
                ));
            }
            Section::Server => {
                let status = if s.server.status.is_empty() { "Unknown".to_owned() } else { s.server.status.clone() };
                let mut rows = vec![
                    Self::row("Status", None, Some(Self::value(status)), false),
                    Self::row(
                        "Version",
                        None,
                        Some(Self::value(s.server.version.clone().unwrap_or_else(|| "Unknown".into()))),
                        false,
                    ),
                ];
                if s.server.has_log {
                    rows.push(Self::row(
                        "Log",
                        None,
                        Some(Self::emit_button("settings-server-log", "Open", SettingsEvent::OpenServerLog, cx)),
                        false,
                    ));
                }
                out.push(Self::group(None, rows));

                let mut actions = div().flex().flex_wrap().gap(px(8.0));
                for (action, label, enabled) in [
                    (ServerAction::Start, "Start", s.server.can_start),
                    (ServerAction::Restart, "Restart…", s.server.can_restart),
                    (ServerAction::Stop, "Stop…", s.server.can_stop),
                ] {
                    if enabled {
                        actions = actions.child(Self::button(
                            ("settings-server-action", action as usize),
                            label,
                            ButtonKind::Secondary,
                            move |this, cx| {
                                if action == ServerAction::Start {
                                    cx.emit(SettingsEvent::Server(action));
                                } else {
                                    this.pending_server = Some(action);
                                    cx.notify();
                                }
                            },
                            cx,
                        ));
                    }
                }
                let control = match self.pending_server {
                    Some(action) => div()
                        .flex()
                        .gap(px(6.0))
                        .child(Self::button(
                            "settings-server-cancel",
                            "Cancel",
                            ButtonKind::Secondary,
                            |this, cx| {
                                this.pending_server = None;
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(Self::button(
                            "settings-server-confirm",
                            if action == ServerAction::Stop { "End Sessions & Stop" } else { "End Sessions & Restart" },
                            ButtonKind::Danger,
                            move |this, cx| {
                                this.pending_server = None;
                                cx.emit(SettingsEvent::Server(action));
                                cx.notify();
                            },
                            cx,
                        ))
                        .into_any_element(),
                    None => actions.into_any_element(),
                };
                let description: gpui::SharedString = if self.pending_server.is_some() {
                    "All sessions will end. Unsaved work will be lost.".into()
                } else {
                    "Restart and Stop end all sessions".into()
                };
                out.push(Self::group(None, vec![Self::row("Server", Some(description), Some(control), narrow)]));
                if s.server.endpoint_id.is_some() || s.server.remote_error.is_some() {
                    out.push(self.devices_group(narrow, cx));
                }
            }
            Section::About => {
                out.push(Self::group(
                    None,
                    vec![
                        Self::row("Version", None, Some(Self::value(s.version.clone())), false),
                        Self::row(
                            "Terminal engine",
                            None,
                            Some({
                                let full = s.revision_full.clone();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(Self::value(s.revision.clone()))
                                    .child(Self::button(
                                        "settings-copy-revision",
                                        "Copy",
                                        ButtonKind::Secondary,
                                        move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(full.clone())),
                                        cx,
                                    ))
                                    .into_any_element()
                            }),
                            false,
                        ),
                    ],
                ));
            }
        }
        out
    }

    /// Settings › Server › Paired devices: the server id, devices with
    /// Revoke, and Pair a device… with its ticket.
    fn devices_group(&self, narrow: bool, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.snapshot.server;
        let copy = |id: &'static str, text: String, cx: &mut Context<Self>| {
            Self::button(id, "Copy", ButtonKind::Secondary, move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())), cx)
        };
        let mut rows = Vec::new();
        if let Some(id) = s.endpoint_id.clone() {
            let short = format!("{}…", id.get(..12).unwrap_or(&id));
            let control = div().flex().items_center().gap(px(6.0)).child(Self::value(short)).child(copy("settings-copy-endpoint", id, cx));
            rows.push(Self::row("Server ID", None, Some(control.into_any_element()), false));
        }
        for (index, device) in s.devices.iter().enumerate() {
            let id = device.id.clone();
            let revoke = Self::emit_button(("settings-revoke", index), "Revoke", SettingsEvent::RevokeDevice(id), cx);
            rows.push(Self::row(device.name.clone(), Some(device.seen.clone().into()), Some(revoke), false));
        }
        if let Some(reason) = s.remote_error.clone() {
            let line = div()
                .id("settings-remote-error")
                .text_size(px(12.5))
                .text_color(rgb(theme::muted()))
                .child("Remote access unavailable")
                .tooltip(crate::tooltip::Tooltip::text(reason));
            rows.push(div().px(px(14.0)).py(px(10.0)).child(line).into_any_element());
        } else {
            let pair = Self::emit_button("settings-pair-device", "Pair a device…", SettingsEvent::PairDevice, cx);
            rows.push(Self::row("New device", Some("Single use, valid 10 minutes".into()), Some(pair), narrow));
        }
        if let Some(ticket) = s.invite.clone() {
            let code = crate::qr::render(&ticket);
            let control = div().flex().items_center().gap(px(6.0)).child(div().max_w(px(180.0)).child(Self::value(ticket.clone()))).child(copy("settings-copy-ticket", ticket, cx));
            rows.push(Self::row("Ticket", None, Some(control.into_any_element()), narrow));
            rows.extend(code.map(|qr| div().w_full().flex().justify_center().py(px(12.0)).child(qr).into_any_element()));
        }
        Self::group(Some("Paired devices"), rows)
    }

    fn sidebar(&self, width: f32, searching: bool, cx: &mut Context<Self>) -> AnyElement {
        // Rounded like the card on its outer side; see `render`.
        let mut items = div().id("settings-sidebar-items").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().pb(px(8.0));
        for (index, section) in Section::ALL.into_iter().enumerate() {
            let selected = self.selected == section && !searching;
            let dimmed = searching && !self.matches(section);
            items = items.child(
                div()
                    .id(("settings-section", index))
                    .h(px(36.0))
                    .mx(px(8.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .continuous(7.0)
                    .cursor_pointer()
                    .when(selected, |s| s.bg(rgb(theme::row_active())))
                    .when(!selected, |s| s.hover(|s| s.bg(rgb(theme::tab_hover()))))
                    .when(dimmed, |s| s.opacity(0.45))
                    .child(
                        div()
                            .size(px(22.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .continuous(6.0)
                            .bg(rgb(section.tint()))
                            .child(icon(section.icon(), 13.0, 0x00ff_ffff)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(13.0))
                            .when(selected, |s| s.font_weight(FontWeight::MEDIUM))
                            .child(section.label()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx))),
            );
        }
        div()
            .w(px(width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(theme::border()))
            .bg(rgb(theme::popover()))
            .continuous_l(crate::radius::concentric(CARD_RADIUS, 1.0))
            // Ends the scrolled list's clip above the bottom corner arc.
            .pb(px(6.0))
            .child(
                div().p(px(10.0)).child(
                    div()
                        .h(px(28.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .continuous(7.0)
                        .bg(rgb(theme::background()))
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .child(icon("settings-search", 13.0, theme::muted()))
                        .child(div().flex_1().min_w_0().overflow_hidden().child(self.search.clone())),
                ),
            )
            .child(items)
            .into_any_element()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Secondary,
    Danger,
}

/// A shortcut hint such as `⇧⌘N` rendered as one keycap per glyph.
fn keycap(shortcut: &str) -> AnyElement {
    let mut caps = div().flex().gap(px(3.0));
    let mut chars = shortcut.chars().peekable();
    while let Some(c) = chars.next() {
        // Multi-letter key names (e.g. "Space") stay together.
        let mut text = c.to_string();
        if c.is_ascii_alphabetic() {
            while let Some(n) = chars.peek().filter(|n| n.is_ascii_alphanumeric()) {
                text.push(*n);
                chars.next();
            }
        }
        caps = caps.child(
            div()
                .min_w(px(20.0))
                .h(px(20.0))
                .px(px(5.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(5.0)
                .bg(rgb(theme::tab_active()))
                .border_1()
                .border_color(rgb(theme::border()))
                .text_size(px(11.5))
                .text_color(rgb(theme::foreground()))
                .child(text),
        );
    }
    caps.into_any_element()
}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The host clamps the card to the viewport; mirror that here.
        let panel_width = (f32::from(window.viewport_size().width) - 64.0).clamp(420.0, 880.0);
        let compact = panel_width < NARROW;
        let sidebar_width = if compact { 168.0 } else { 200.0 };
        let narrow = panel_width - sidebar_width < 480.0;
        let searching = !self.query.is_empty();

        let sections: Vec<_> = Section::ALL
            .into_iter()
            .filter(|section| if searching { self.matches(*section) } else { *section == self.selected })
            .collect();
        let mut content = div().w_full().max_w(px(620.0)).flex().flex_col().gap(px(14.0)).pb(px(28.0));
        if let Some(message) = self.snapshot.message.clone() {
            content = content.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .continuous(8.0)
                    .bg(rgb(theme::tab_hover()))
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child(message),
            );
        }
        if sections.is_empty() {
            content = content.child(
                div().pt(px(40.0)).flex().justify_center().text_color(rgb(theme::muted())).child("No matching settings"),
            );
        }
        for (i, section) in sections.into_iter().enumerate() {
            // The toolbar names a single page; search results get headings.
            if searching {
                let heading = div()
                    .when(i > 0, |h| h.pt(px(12.0)))
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(section.label());
                content = content.child(heading);
            }
            content = content.children(self.page(section, narrow, cx));
        }
        let title = if searching { "Search Results" } else { self.selected.label() };
        let gutter = if compact { 20.0 } else { 28.0 };

        div()
            .id("settings-panel")
            .key_context("Settings")
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .flex()
            .overflow_hidden()
            // The host card's content mask is rectangular (GPUI clips to
            // bounds, not to the corner radius), so the panel's own fills
            // must round to the card's inner radius or they square it off.
            .continuous(crate::radius::concentric(CARD_RADIUS, 1.0))
            .occlude()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .text_color(rgb(theme::foreground()))
            .bg(rgb(theme::background()))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if this.theme_menu_key(&event.keystroke.key, cx) {
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" {
                    cx.emit(SettingsEvent::Close);
                    cx.stop_propagation();
                }
            }))
            .child(self.sidebar(sidebar_width, searching, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    // Fixed toolbar: page title and close button stay put
                    // and the scrolled content is clipped below it, clear
                    // of the card's rounded corners and border.
                    .child(
                        div()
                            .flex_none()
                            .h(px(TOOLBAR_H))
                            .pl(px(gutter))
                            .pr(px(10.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .border_b_1()
                            .border_color(rgb(theme::border()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .id("settings-close")
                                    .flex_none()
                                    .size(px(24.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .continuous(12.0)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(theme::tab_hover())))
                                    .child(icon("close", 12.0, theme::muted()))
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
                            ),
                    )
                    .child(
                        div()
                            .id("settings-details")
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            // Menus are anchored where they opened; close
                            // them rather than leave them stale on scroll.
                            .on_scroll_wheel(cx.listener(|this, _: &gpui::ScrollWheelEvent, window, cx| {
                                if this.theme_menu.take().is_some() {
                                    cx.notify();
                                }
                                this.close_font_menu(window, cx);
                            }))
                            .px(px(gutter))
                            .pt(px(18.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(content),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_only_offer_supported_layout_preferences() {
        let snapshot = SettingsSnapshot::default();
        assert_eq!(snapshot.density, Density::Comfortable);
        assert!(snapshot.show_pane_headers);
        assert!(!snapshot.server.can_start && !snapshot.server.can_stop && !snapshot.server.can_restart);
        assert!(snapshot.shortcuts.is_empty());
    }

    #[test]
    fn font_picker_lists_automatic_then_installed_and_flags_missing() {
        let mut t = SettingsSnapshot::default().terminal;
        t.font_choices = vec!["Berkeley Mono Nerd Font Mono".into(), "Menlo".into(), "Monaco".into()];
        t.font_automatic = "Berkeley Mono Nerd Font Mono".into();
        t.font_automatic_nerd = true;
        t.font_in_use = t.font_automatic.clone();
        assert_eq!(t.font_family, "", "Automatic by default");
        assert_eq!(t.font_entries("").len(), 4);
        assert_eq!(t.font_entries("")[0], None);
        assert!(t.font_selected(None));
        assert_eq!(t.font_status().as_deref(), Some("Using Berkeley Mono Nerd Font Mono"));
        assert_eq!(t.font_entries("mona"), vec![Some("Monaco".into())]);
        // Automatic also matches the family it resolves to.
        assert_eq!(t.font_entries("berkeley"), vec![None, Some("Berkeley Mono Nerd Font Mono".into())]);
        assert_eq!(t.font_entries("nerd"), vec![None, Some("Berkeley Mono Nerd Font Mono".into())]);
        assert!(t.font_entries("zzz").is_empty());

        t.font_family = "menlo".into();
        assert!(t.font_selected(Some("Menlo")) && !t.font_selected(None));
        assert_eq!(t.font_status(), None);

        t.font_family = "Removed Mono".into();
        t.font_missing = true;
        assert_eq!(t.font_status().as_deref(), Some("Not installed; using Berkeley Mono Nerd Font Mono"));

        t.font_family = String::new();
        t.font_missing = false;
        t.font_automatic_nerd = false;
        t.font_in_use = "Menlo".into();
        assert_eq!(t.font_status().as_deref(), Some("No Nerd Font installed; using Menlo"));
    }

    #[test]
    fn sidebar_sections_have_search_terms_and_icons() {
        assert_eq!(Section::ALL.len(), 6);
        assert_eq!(Section::ALL[0], Section::Appearance, "Settings opens on Appearance");
        assert!(!Section::ALL.iter().any(|s| s.label() == "General"));
        for section in Section::ALL {
            assert!(!section.keywords().is_empty());
            assert!(crate::assets::has_icon(section.icon()), "missing icon {}", section.icon());
        }
        assert!(crate::assets::has_icon("settings-search"));
    }

    #[test]
    fn ui_font_picker_is_separate_from_the_terminal_font() {
        let snapshot = SettingsSnapshot::default();
        let mut ui = snapshot.ui_font.clone();
        assert_eq!(ui.family, "", "Automatic by default");
        assert!(ui.font_selected(None));
        assert_eq!(ui.font_status().as_deref(), Some("System font"));
        ui.choices = vec!["Helvetica Neue".into(), "Inter".into(), "Menlo".into()];
        assert_eq!(ui.font_entries(""), vec![None, Some("Helvetica Neue".into()), Some("Inter".into()), Some("Menlo".into())]);
        assert_eq!(ui.font_entries("system"), vec![None]);
        assert_eq!(ui.font_entries("int"), vec![Some("Inter".into())]);
        assert!(ui.font_entries("zzz").is_empty());

        ui.family = "inter".into();
        assert!(ui.font_selected(Some("Inter")) && !ui.font_selected(None));
        assert_eq!(ui.font_status(), None);
        // Choosing a UI font leaves the terminal preference alone.
        assert_eq!(snapshot.terminal.font_family, "");

        ui.family = "Removed Sans".into();
        ui.missing = true;
        assert_eq!(ui.font_status().as_deref(), Some("Not installed; using the system font"));
    }

    #[test]
    fn font_searches_find_appearance_and_terminal() {
        for query in ["font", "ui font", "interface", "typeface"] {
            let text = format!("{} {}", Section::Appearance.label(), Section::Appearance.keywords());
            assert!(query.split_whitespace().all(|w| text.to_lowercase().contains(w)), "{query} misses Appearance");
        }
        assert!(Section::Terminal.keywords().contains("font"));
    }
}
