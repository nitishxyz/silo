//! Snapshot-driven settings UI. The owner applies events and refreshes the snapshot;
//! rendering never reads files, changes preferences, or controls the daemon.
//!
//! Layout follows a system-settings pattern: a sidebar of icon tiles on the
//! left, the selected page on the right made of grouped, aligned rows.

use gpui::{
    Anchor, AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, FontWeight, IntoElement, Render, ScrollHandle, Subscription,
    Window, anchored, deferred, div, prelude::*, px, rgb,
};

pub use crate::hosts::Density;
pub use crate::terminal_prefs::ColorSlot;
use crate::theme::Continuous as _;
pub use silo_proto::model::CursorShape;
use silo_proto::permissions::{PermissionId, PermissionStatus, Permissions};

type TriggerBounds = std::rc::Rc<std::cell::Cell<Option<(gpui::Bounds<gpui::Pixels>, gpui::Size<gpui::Pixels>)>>>;

/// The font menu's anchor and window position: left-aligned under the
/// trigger, or above it when it does not fit below and there is more room
/// above.
fn font_menu_place(
    trigger: gpui::Bounds<gpui::Pixels>,
    viewport: gpui::Size<gpui::Pixels>,
    menu_height: gpui::Pixels,
) -> (Anchor, gpui::Point<gpui::Pixels>) {
    const GAP: f32 = 4.0;
    const MARGIN: f32 = 8.0;
    let below = viewport.height - trigger.bottom() - px(GAP + MARGIN);
    let above = trigger.top() - px(GAP + MARGIN);
    if below < menu_height && above > below {
        (Anchor::BottomLeft, gpui::point(trigger.left(), trigger.top() - px(GAP)))
    } else {
        (Anchor::TopLeft, gpui::point(trigger.left(), trigger.bottom() + px(GAP)))
    }
}
use crate::{
    assets::icon,
    controls::{Segmented, Toggle},
    settings_table::SettingId,
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
    /// This device shows the host's notifications.
    pub notify: bool,
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
    /// launchd runs the server (in the background, at login).
    pub managed: bool,
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
    pub pane_headers: crate::pane_header::PaneHeaders,
    pub match_running_app: bool,
    pub material: crate::material::Material,
    /// Why Frosted cannot render now (the choice is kept).
    pub material_unavailable: Option<&'static str>,
    pub tint_with_app: bool,
    pub translucent_apps: bool,
    /// This build updates itself (a release Silo.app in Applications).
    pub updates: bool,
    pub check_updates: bool,
    /// Ask before a close that would end running programs.
    pub confirm_close: bool,
    /// What the address bar searches with.
    pub search_engine: crate::search::SearchEngine,
    pub notify: crate::hosts::NotifyPrefs,
    /// macOS refused Silo's notifications.
    pub notify_denied: bool,
    pub ui_font: UiFontSettings,
    pub terminal: TerminalSettings,
    pub hosts: Vec<SettingsHost>,
    pub shortcuts: Vec<SettingsShortcut>,
    pub server: SettingsServer,
    /// The window shows this Mac's host: the Permissions page is for it.
    pub local_host: bool,
    /// Silo Server's macOS privacy permissions (`permissions.status`),
    /// or why they can't be read; `None` until first read.
    pub permissions: Option<Result<Permissions, String>>,
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
            pane_headers: crate::pane_header::PaneHeaders::default(),
            match_running_app: true,
            material: crate::material::Material::Solid,
            material_unavailable: None,
            tint_with_app: true,
            translucent_apps: false,
            updates: true,
            check_updates: true,
            confirm_close: true,
            search_engine: crate::search::SearchEngine::default(),
            notify: crate::hosts::NotifyPrefs::default(),
            notify_denied: false,
            ui_font: UiFontSettings::default(),
            terminal: TerminalSettings::from_prefs(&TerminalPrefs::default(), terminal_prefs::DEFAULT_FONT.into()),
            hosts: Vec::new(),
            shortcuts: Vec::new(),
            server: SettingsServer::default(),
            local_host: true,
            permissions: None,
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
    SetSearchEngine(crate::search::SearchEngine),
    /// Show a block type's headers.
    SetPaneHeaders(crate::pane_header::HeaderBlock, bool),
    /// Card padding and chrome take on the running app's background.
    SetMatchRunningApp(bool),
    SetMaterial(crate::material::Material),
    SetTintWithApp(bool),
    SetTranslucentApps(bool),
    SetCheckUpdates(bool),
    SetConfirmClose(bool),
    SetNotify(crate::hosts::NotifyField, bool),
    /// A host's notifications on this device (host id, on).
    SetHostNotify(String, bool),
    /// System Settings › Notifications.
    OpenNotificationSettings,
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
    /// Restart… / Stop…: the workspace asks what runs first, then shows
    /// either its close dialog or the panel's confirmation
    /// ([`SettingsPanel::confirm_server`]), never both.
    AskServer(ServerAction),
    /// `pair.create`: show a new pairing ticket.
    PairDevice,
    RevokeDevice(String),
    /// Re-read the permissions (their page was shown).
    RefreshPermissions,
    /// Help grant one: the Permissions panel at it (`permissions.rs`).
    GrantPermission(PermissionId),
    /// System Settings › Privacy & Security › Automation.
    OpenAutomationSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    Appearance,
    Keyboard,
    Notifications,
    Terminal,
    Browser,
    Hosts,
    Server,
    Permissions,
    About,
}

/// The sidebar's groups, by what their pages set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    General,
    Terminal,
    Browser,
    Connections,
    About,
}

impl Group {
    /// The sidebar label; About stands alone at the bottom.
    fn label(self) -> Option<&'static str> {
        match self {
            Self::General => Some("General"),
            Self::Terminal => Some("Terminal"),
            Self::Browser => Some("Browser"),
            Self::Connections => Some("Connections"),
            Self::About => None,
        }
    }
}

/// A sidebar row: a group's label or a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarRow {
    Label(Group),
    Section(Section),
}

/// The sidebar's rows, in [`Section::ALL`] order: each shown page under its
/// group's label. `shown` filters the pages (a search), and a group with
/// none shown loses its label too.
fn sidebar_rows(shown: impl Fn(Section) -> bool) -> Vec<SidebarRow> {
    let mut rows = Vec::new();
    let mut group = None;
    for section in Section::ALL.into_iter().filter(|s| shown(*s)) {
        if group != Some(section.group()) {
            group = Some(section.group());
            if section.group().label().is_some() {
                rows.push(SidebarRow::Label(section.group()));
            }
        }
        rows.push(SidebarRow::Section(section));
    }
    rows
}

impl Section {
    const ALL: [Self; 9] = [
        Self::Appearance,
        Self::Keyboard,
        Self::Notifications,
        Self::Terminal,
        Self::Browser,
        Self::Hosts,
        Self::Server,
        Self::Permissions,
        Self::About,
    ];

    /// The page labelled `name` (any case).
    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.label().eq_ignore_ascii_case(name.trim()))
    }

    pub(crate) fn group(self) -> Group {
        match self {
            Self::Appearance | Self::Keyboard | Self::Notifications => Group::General,
            Self::Terminal => Group::Terminal,
            Self::Browser => Group::Browser,
            Self::Hosts | Self::Server | Self::Permissions => Group::Connections,
            Self::About => Group::About,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
            Self::Keyboard => "Keyboard",
            Self::Notifications => "Notifications",
            Self::Hosts => "Hosts",
            Self::Server => "Server",
            Self::Permissions => "Permissions",
            Self::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Appearance => "settings-shapes",
            Self::Terminal => "terminal",
            Self::Browser => "globe",
            Self::Keyboard => "settings-keyboard",
            Self::Notifications => "settings-bell",
            Self::Hosts => "laptop",
            Self::Server => "settings-hard-drive",
            Self::Permissions => "lock",
            Self::About => "settings-info",
        }
    }

    /// Tile color behind the sidebar icon.
    fn tint(self) -> u32 {
        match self {
            Self::Appearance => 0x008b_5cf6,
            Self::Terminal => 0x0014_b8a6,
            Self::Browser => 0x0022_c55e,
            Self::Keyboard => 0x0063_66f1,
            Self::Notifications => 0x00ef_4444,
            Self::Hosts => 0x000e_a5e9,
            Self::Server => 0x00f5_9e0b,
            Self::Permissions => 0x0064_748b,
            Self::About => 0x0000_78d4,
        }
    }

    /// Search words: the page's own and its settings' (`settings_table`).
    fn keywords(self) -> String {
        let own = match self {
            Self::Appearance => "general preferences layout theme colors",
            Self::Terminal => "preview settings file json configuration config edit open reload finder",
            Self::Browser => "browser web pages swipe back forward zoom pinch search engine address bar",
            Self::Keyboard => "shortcuts keys commands bindings",
            Self::Notifications => "alerts banners dock badge needs you done error",
            Self::Hosts => "remote local address add remove iroh notifications notify",
            Self::Server => "daemon status version log start stop restart sessions",
            Self::Permissions => {
                "privacy security macos system settings grant tcc screen recording capture accessibility full disk access files input monitoring keyboard automation silo server"
            }
            Self::About => "silo version revision build libghostty",
        };
        format!("{own} {}", crate::settings_table::section_keywords(self))
    }
}

/// Whether a search for `words` (lowercase) shows `section`.
fn section_matches(section: Section, snapshot: &SettingsSnapshot, words: &[&str]) -> bool {
    let mut text = format!("{} {}", section.label(), section.keywords());
    if matches!(section, Section::Terminal | Section::Appearance) {
        for choice in &snapshot.terminal.themes {
            text.push_str(&format!(" {}", choice.name));
        }
    }
    if section == Section::Keyboard {
        for shortcut in &snapshot.shortcuts {
            text.push_str(&format!(" {} {}", shortcut.label, shortcut.shortcut));
        }
    }
    if section == Section::Hosts {
        for host in &snapshot.hosts {
            text.push_str(&format!(" {} {}", host.name, host.address));
        }
    }
    let text = text.to_lowercase();
    words.iter().all(|word| text.contains(word))
}

/// Where a panel laid out its titled groups, by page and title (`None`,
/// "" for the page content), as window y from the last prepaint: where
/// [`SettingsPanel::reveal`] scrolls to. One per panel.
#[derive(Clone, Default)]
struct Places(std::rc::Rc<std::cell::RefCell<std::collections::HashMap<PlaceKey, f32>>>);

/// A page (`None`: the page content) and a group title.
type PlaceKey = (Option<Section>, &'static str);

impl Places {
    fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Where group `title` of page `section` sits below the content top.
    fn offset(&self, section: Section, title: &'static str) -> Option<f32> {
        let places = self.0.borrow();
        Some(places.get(&(Some(section), title))? - places.get(&(None, ""))?)
    }

    /// Records where `key` is laid out.
    fn probe(&self, key: PlaceKey) -> impl IntoElement + use<> {
        let places = self.0.clone();
        gpui::canvas(
            move |bounds, _, _| {
                places.borrow_mut().insert(key, f32::from(bounds.top()));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

pub struct SettingsPanel {
    snapshot: SettingsSnapshot,
    selected: Section,
    /// A setting to scroll to once its page is laid out, and frames waited.
    reveal: Option<(SettingId, u8)>,
    /// Where groups were laid out, and the page being built.
    places: Places,
    building: std::cell::Cell<Section>,
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
    /// Window bounds of each font trigger (terminal, UI) and the viewport
    /// size, from the last prepaint: where the font menu opens.
    font_triggers: [TriggerBounds; 2],
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
            FieldEvent::Submit => match SettingId::Scrollback.parse(&field.read(cx).text) {
                Ok(event) => {
                    this.field_error = None;
                    cx.emit(event);
                }
                Err(error) => {
                    this.field_error = Some(error);
                    cx.notify();
                }
            },
            FieldEvent::Cancel => this.sync_fields(cx),
            _ => {}
        }));
        let mut this = Self {
            snapshot,
            selected: Section::Appearance,
            reveal: None,
            places: Places::default(),
            building: std::cell::Cell::new(Section::Appearance),
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
            font_triggers: Default::default(),
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

    /// Whether the Permissions page is on screen.
    #[must_use]
    pub fn showing_permissions(&self) -> bool {
        self.shown(Section::Permissions) && (self.selected == Section::Permissions || (!self.query.is_empty() && self.matches(Section::Permissions)))
    }

    /// Whether `section` is in the sidebar: Permissions only for this Mac
    /// (its own server's permissions), on macOS.
    fn shown(&self, section: Section) -> bool {
        section != Section::Permissions || (cfg!(target_os = "macos") && self.snapshot.local_host)
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.search.read(cx).focus.clone();
        focus.focus(window, cx);
    }

    /// Ask to confirm a server action inline (nothing is running).
    pub fn confirm_server(&mut self, action: ServerAction, cx: &mut Context<Self>) {
        self.pending_server = Some(action);
        cx.notify();
    }

    /// Show `setting`'s page scrolled to it.
    pub fn reveal(&mut self, setting: SettingId, cx: &mut Context<Self>) {
        self.select(setting.setting().section, cx);
        self.places.clear();
        self.reveal = Some((setting, 0));
    }

    /// Show `section`'s page (a block header's settings button).
    pub(crate) fn show_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.select(section, cx);
    }

    /// Scroll to the setting being revealed once its page was laid out.
    fn scroll_to_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((setting, frames)) = self.reveal else { return };
        let Some(group) = setting.setting().group else {
            self.reveal = None;
            return;
        };
        match self.places.offset(setting.setting().section, group) {
            Some(y) => {
                self.scroll.set_offset(gpui::point(px(0.0), px(-(y - 6.0).max(0.0))));
                self.reveal = None;
            }
            None if frames < 10 => {
                self.reveal = Some((setting, frames + 1));
                cx.on_next_frame(window, |_, _, cx| cx.notify());
            }
            None => self.reveal = None,
        }
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
        if section == Section::Permissions {
            cx.emit(SettingsEvent::RefreshPermissions);
        }
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
        self.shown(section) && section_matches(section, &self.snapshot, &self.words().collect::<Vec<_>>())
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
            ButtonKind::Secondary => (theme::control_fill(), theme::foreground(), theme::row_fill()),
            ButtonKind::Danger => (rgb(theme::danger()), 0x00ff_ffff, rgb(theme::danger())),
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
            .bg(bg)
            .text_color(rgb(fg))
            .when(kind == ButtonKind::Secondary, |b| b.border_1().border_color(rgb(theme::border())))
            .cursor_pointer()
            .hover(move |s| if kind == ButtonKind::Secondary { s.bg(hover) } else { s.opacity(0.88) })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
            .into_any_element()
    }

    fn emit_button(id: impl Into<gpui::ElementId>, label: &'static str, event: SettingsEvent, cx: &mut Context<Self>) -> AnyElement {
        Self::button(id, label, ButtonKind::Secondary, move |_, cx| cx.emit(event.clone()), cx)
    }

    /// `terminal.json` with reveal, edit and reload icon buttons.
    fn config_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let action = |id: &'static str, glyph: &'static str, tip: &'static str, event: SettingsEvent, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(6.0)
                .cursor_pointer()
                .hover(|s| s.bg(theme::hover_fill()))
                .tooltip(crate::tooltip::Tooltip::text(tip))
                .child(icon(glyph, 13.0, theme::muted()))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(event.clone())))
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                div()
                    .flex_1()
                    .font_family(self.snapshot.terminal.font_in_use.clone())
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child(terminal_prefs::FILE),
            )
            .child(action("settings-terminal-reveal", "settings-search", "Show in Finder", SettingsEvent::RevealConfig, cx))
            .child(action("settings-terminal-config", "forward", "Edit", SettingsEvent::OpenConfig, cx))
            .child(action("settings-terminal-reload", "reload", "Reload", SettingsEvent::ReloadTerminal, cx))
            .into_any_element()
    }

    /// One aligned preference row: title (and optional description) on the
    /// left, control on the right; stacks when the content is narrow.
    fn row(title: impl Into<gpui::SharedString>, description: Option<gpui::SharedString>, control: Option<AnyElement>, narrow: bool) -> AnyElement {
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
                    .children(description.map(|d| div().text_size(px(11.5)).line_height(px(15.0)).text_color(rgb(theme::muted())).child(d))),
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

    /// A titled group records where it is laid out ([`Places`]) under the
    /// page being built ([`Self::page`]).
    fn group(&self, title: Option<&'static str>, rows: Vec<AnyElement>) -> AnyElement {
        let count = rows.len();
        let mut card =
            div().w_full().flex().flex_col().overflow_hidden().continuous(10.0).border_1().border_color(rgb(theme::border())).bg(theme::group_fill());
        for (index, row) in rows.into_iter().enumerate() {
            card = card.child(row);
            if index + 1 < count {
                card = card.child(div().mx(px(14.0)).h(px(1.0)).bg(rgb(theme::border())));
            }
        }
        div()
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(title.map(|t| div().px(px(4.0)).text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(theme::muted())).child(t)))
            .child(card)
            .children(title.map(|t| self.places.probe((Some(self.building.get()), t))))
            .into_any_element()
    }

    fn note(text: &'static str) -> AnyElement {
        div().px(px(4.0)).text_size(px(11.5)).line_height(px(16.0)).text_color(rgb(theme::muted())).child(text).into_any_element()
    }

    /// The kit-style animated switch; reports the requested state.
    fn switch(id: &'static str, label: &'static str, on: bool, event: fn(bool) -> SettingsEvent, cx: &mut Context<Self>) -> AnyElement {
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

    fn material_control(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::material::Material;
        let panel = cx.entity().downgrade();
        let selected = usize::from(self.snapshot.material == Material::Frosted);
        Segmented::new("settings-material", ["Solid", "Frosted"], selected)
            .segment_width(80.0)
            .when_some(self.snapshot.material_unavailable, |s, why| s.unavailable(1, why))
            .on_select(move |index, _, cx| {
                let material = if index == 1 { Material::Frosted } else { Material::Solid };
                let _ = panel.update(cx, |_, cx| cx.emit(SettingsEvent::SetMaterial(material)));
            })
            .into_any_element()
    }

    fn search_engine_control(&self, cx: &mut Context<Self>) -> AnyElement {
        const ENGINES: [(crate::search::SearchEngine, &str); 4] = {
            use crate::search::SearchEngine as E;
            [(E::Google, E::Google.label()), (E::DuckDuckGo, E::DuckDuckGo.label()), (E::Bing, E::Bing.label()), (E::Kagi, E::Kagi.label())]
        };
        Self::segmented("settings-search-engine", &ENGINES, self.snapshot.search_engine, 84.0, SettingsEvent::SetSearchEngine, cx)
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
            .bg(theme::well_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(field.clone())
            .into_any_element()
    }

    fn swatch(color: u32, size: f32) -> AnyElement {
        div().flex_none().size(px(size)).continuous(5.0).bg(rgb(color)).border_1().border_color(rgb(theme::border())).into_any_element()
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
                CursorShape::Block | CursorShape::Unknown => base.bg(rgb(cursor)),
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
            .bg(theme::control_fill())
            .border_1()
            .border_color(rgb(if open { theme::accent() } else { theme::border() }))
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
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
                .bg(theme::overlay_fill())
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
                        .when(highlighted, |r| r.bg(theme::row_fill()))
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
                div()
                    .absolute()
                    .top(px(32.0))
                    .right_0()
                    .child(deferred(anchored().anchor(Anchor::TopRight).snap_to_window_with_margin(px(8.0)).child(list)).with_priority(1)),
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
        self.font_highlight = self.font_entries(target, "").iter().position(|e| self.font_selected(target, e.as_deref())).unwrap_or(0);
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
            .bg(theme::control_fill())
            .border_1()
            .border_color(rgb(border))
            .cursor_pointer()
            .hover(|s| s.bg(theme::row_fill()))
            .child(div().flex_1().min_w_0().text_ellipsis().whitespace_nowrap().overflow_hidden().text_size(px(12.5)).child(label))
            .child(icon("chevron", 11.0, theme::muted()))
            .on_click(cx.listener(move |this, _, window, cx| this.open_font_menu(target, window, cx)));

        let placed = self.font_triggers[usize::from(target == FontTarget::Ui)].clone();
        let last = placed.get();
        let mut root = div().relative().child(trigger).on_children_prepainted(move |bounds, window, _| {
            let now = bounds.first().map(|b| (*b, window.viewport_size()));
            if placed.get() != now {
                placed.set(now);
                window.refresh();
            }
        });
        if open {
            let entries = self.font_entries(target, &self.font_query);
            #[allow(clippy::cast_precision_loss)]
            let menu_height = px(36.0 + (entries.len().max(1) as f32 * 26.0 + 8.0).min(264.0));
            let mut list =
                div().id("settings-font-list").max_h(px(264.0)).overflow_y_scroll().track_scroll(&self.font_scroll).p(px(4.0)).flex().flex_col();
            if entries.is_empty() {
                list = list.child(div().px(px(8.0)).py(px(6.0)).text_size(px(12.0)).text_color(rgb(theme::muted())).child("No matching fonts"));
            }
            for (index, entry) in entries.into_iter().enumerate() {
                let selected = self.font_selected(target, entry.as_deref());
                let highlighted = index == self.font_highlight;
                let (name, detail, face): (String, Option<String>, gpui::SharedString) = match (&entry, target) {
                    (None, FontTarget::Terminal) => ("Automatic".into(), Some(self.snapshot.terminal.font_automatic.clone()), theme::ui_font()),
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
                        .when(highlighted, |r| r.bg(theme::row_fill()))
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
                .w(last.map_or(px(260.0), |(b, _)| b.size.width.max(px(260.0))))
                .flex()
                .flex_col()
                .overflow_hidden()
                .continuous(9.0)
                .bg(theme::overlay_fill())
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
            let mut place = anchored().snap_to_window_with_margin(px(8.0));
            if let Some((trigger, viewport)) = last {
                let (anchor, at) = font_menu_place(trigger, viewport, menu_height);
                place = place.position_mode(gpui::AnchoredPositionMode::Window).anchor(anchor).position(at);
            }
            root = root.child(div().absolute().top(px(32.0)).left_0().child(deferred(place.child(menu)).with_priority(1)));
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
        out.push(self.config_line(cx));

        out.push(self.group(None, vec![Self::row(SettingId::Theme.label(), None, Some(self.theme_dropdown("settings-terminal-theme", cx)), false)]));

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
            Self::row(SettingId::FontFamily.label(), t.font_status().map(Into::into), Some(self.font_dropdown(FontTarget::Terminal, cx)), narrow),
            Self::row(SettingId::FontSize.label(), Some("This Mac only".into()), Some(size_control), narrow),
        ];
        out.push(self.group(Some("Font"), font_rows));

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
            color_rows.push(Self::row(SettingId::Color(slot).label(), description, Some(control.into_any_element()), narrow));
        }
        out.push(self.group(Some("Colors"), color_rows));

        let blink = t.cursor_blink;
        out.push(self.group(
            Some("Cursor"),
            vec![
                Self::row(
                    SettingId::CursorShape.label(),
                    None,
                    Some(Self::segmented(
                        "settings-cursor-shape",
                        &crate::settings_table::CURSOR_SHAPES,
                        t.cursor_shape,
                        76.0,
                        SettingsEvent::SetCursorShape,
                        cx,
                    )),
                    narrow,
                ),
                Self::row(
                    SettingId::CursorBlink.label(),
                    None,
                    Some(Self::switch("settings-cursor-blink", "Cursor blinking", blink, SettingsEvent::SetCursorBlink, cx)),
                    false,
                ),
            ],
        ));

        out.push(self.group(
            Some("Closing"),
            vec![Self::row(
                SettingId::ConfirmClose.label(),
                Some("Closing a pane, tab or session asks first when a program is running".into()),
                Some(Self::switch(
                    "settings-confirm-close",
                    "Confirm before closing running programs",
                    self.snapshot.confirm_close,
                    SettingsEvent::SetConfirmClose,
                    cx,
                )),
                false,
            )],
        ));

        out.push(self.group(
            Some("Scrollback"),
            vec![Self::row(
                SettingId::Scrollback.label(),
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

        out.push(self.group(
            None,
            vec![Self::row(
                SettingId::RestoreDefaults.label(),
                None,
                Some(Self::emit_button("settings-terminal-reset", "Restore", SettingsEvent::ResetTerminal, cx)),
                false,
            )],
        ));
        out
    }

    // ---- pages ----

    #[allow(clippy::too_many_lines)]
    fn page(&self, section: Section, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.building.set(section);
        let s = &self.snapshot;
        let mut out = Vec::new();
        match section {
            Section::Appearance => {
                out.push(self.group(Some("Layout"), vec![Self::row(SettingId::PaneDensity.label(), None, Some(self.density_control(cx)), narrow)]));
                {
                    use crate::pane_header::HeaderBlock;
                    let headers = s.pane_headers;
                    out.push(self.group(
                        Some("Block headers"),
                        vec![
                            Self::row(
                                SettingId::TerminalHeaders.label(),
                                None,
                                Some(Self::switch(
                                    "settings-headers-terminal",
                                    "Terminal headers",
                                    headers.terminal,
                                    |on| SettingsEvent::SetPaneHeaders(HeaderBlock::Terminal, on),
                                    cx,
                                )),
                                false,
                            ),
                            Self::row(
                                SettingId::AgentHeaders.label(),
                                Some("Browsers always show their address bar".into()),
                                Some(Self::switch(
                                    "settings-headers-agent",
                                    "Agent headers",
                                    headers.agent,
                                    |on| SettingsEvent::SetPaneHeaders(HeaderBlock::Agent, on),
                                    cx,
                                )),
                                false,
                            ),
                        ],
                    ));
                }
                let mut colors = vec![Self::row(SettingId::Theme.label(), None, Some(self.theme_dropdown("settings-appearance-theme", cx)), false)];
                if SettingId::MatchRunningApp.visible(s) {
                    colors.push(Self::row(
                        SettingId::MatchRunningApp.label(),
                        Some("Window and padding take on the background of the focused app or page".into()),
                        Some(Self::switch("settings-match-app", "Match running app", s.match_running_app, SettingsEvent::SetMatchRunningApp, cx)),
                        false,
                    ));
                }
                out.push(self.group(Some("Colors"), colors));
                let unavailable = s.material_unavailable.filter(|_| s.material == crate::material::Material::Frosted);
                let mut material =
                    vec![Self::row(SettingId::WindowMaterial.label(), unavailable.map(Into::into), Some(self.material_control(cx)), narrow)];
                if SettingId::TintWithApp.visible(s) {
                    material.push(Self::row(
                        SettingId::TintWithApp.label(),
                        Some("The blur takes on the background of the focused app or page".into()),
                        Some(Self::switch("settings-tint-app", "Tint with running app", s.tint_with_app, SettingsEvent::SetTintWithApp, cx)),
                        false,
                    ));
                }
                if SettingId::TranslucentApps.visible(s) {
                    material.push(Self::row(
                        SettingId::TranslucentApps.label(),
                        Some("Full-screen apps show the blur through their own background".into()),
                        Some(Self::switch(
                            "settings-translucent-apps",
                            "Translucent app backgrounds",
                            s.translucent_apps,
                            SettingsEvent::SetTranslucentApps,
                            cx,
                        )),
                        false,
                    ));
                }
                out.push(self.group(Some("Material"), material));
                out.push(self.group(
                    Some("Font"),
                    vec![Self::row(
                        SettingId::InterfaceFont.label(),
                        s.ui_font.font_status().map(Into::into),
                        Some(self.font_dropdown(FontTarget::Ui, cx)),
                        narrow,
                    )],
                ));
            }
            Section::Terminal => out.extend(self.terminal_page(narrow, cx)),
            Section::Browser => {
                out.push(self.group(
                    Some("Address bar"),
                    vec![Self::row(
                        SettingId::SearchEngine.label(),
                        Some("What the address bar searches with when you don't type an address".into()),
                        Some(self.search_engine_control(cx)),
                        narrow,
                    )],
                ));
                out.push(self.group(
                    Some("Gestures"),
                    vec![Self::row(
                        "Swipe between pages",
                        Some("Two fingers go back or forward, as set in macOS".into()),
                        Some(Self::value("System".to_owned())),
                        false,
                    )],
                ));
                out.push(Self::note("An agent's web search engine is in its Otto settings."));
            }
            Section::Notifications => out.extend(self.notifications_page(cx)),
            Section::Keyboard => {
                let show_all = self.query.is_empty() || self.text_matches(&format!("{} {}", section.label(), section.keywords()));
                let rows: Vec<AnyElement> = s
                    .shortcuts
                    .iter()
                    .filter(|k| show_all || self.text_matches(&format!("{} {}", k.label, k.shortcut)))
                    .map(|k| Self::row(k.label.clone(), None, Some(keycap(&k.shortcut)), false))
                    .collect();
                if rows.is_empty() {
                    out.push(Self::note("No shortcuts."));
                } else {
                    out.push(self.group(None, rows));
                }
            }
            Section::Hosts => {
                let mut rows = Vec::new();
                for (index, host) in s.hosts.iter().enumerate() {
                    let confirming = self.pending_remove.as_deref() == Some(host.id.as_str());
                    let notify = (!confirming).then(|| self.host_notify(index, host, cx));
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
                    let control = match (notify, control) {
                        (Some(notify), Some(control)) => {
                            Some(div().flex().items_center().gap(px(10.0)).child(notify).child(control).into_any_element())
                        }
                        (notify, control) => notify.or(control),
                    };
                    rows.push(Self::row(host.name.clone(), Some(host.status.clone().into()), control, narrow));
                }
                if !rows.is_empty() {
                    out.push(self.group(None, rows));
                }
                out.push(self.group(
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
                    Self::row(
                        "Status",
                        s.server.managed.then(|| "Runs in the background · starts at login".into()),
                        Some(Self::value(status)),
                        false,
                    ),
                    Self::row("Version", None, Some(Self::value(s.server.version.clone().unwrap_or_else(|| "Unknown".into()))), false),
                ];
                if s.server.has_log {
                    rows.push(Self::row(
                        "Log",
                        None,
                        Some(Self::emit_button("settings-server-log", "Open", SettingsEvent::OpenServerLog, cx)),
                        false,
                    ));
                }
                out.push(self.group(None, rows));

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
                            move |_, cx| {
                                cx.emit(if action == ServerAction::Start { SettingsEvent::Server(action) } else { SettingsEvent::AskServer(action) });
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
                out.push(self.group(None, vec![Self::row("Server", Some(description), Some(control), narrow)]));
                if s.server.endpoint_id.is_some() || s.server.remote_error.is_some() {
                    out.push(self.devices_group(narrow, cx));
                }
            }
            Section::Permissions => out.extend(self.permissions_page(narrow, cx)),
            Section::About => {
                out.push(self.group(
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
                if SettingId::CheckUpdates.visible(s) {
                    out.push(self.group(
                        Some("Updates"),
                        vec![Self::row(
                            SettingId::CheckUpdates.label(),
                            None,
                            Some(Self::switch(
                                "settings-check-updates",
                                "Check for updates automatically",
                                s.check_updates,
                                SettingsEvent::SetCheckUpdates,
                                cx,
                            )),
                            false,
                        )],
                    ));
                }
            }
        }
        out
    }

    /// Settings › Notifications: on/off, which reasons, and only for blocks
    /// not on screen. Each host has its own switch in Hosts.
    fn notifications_page(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::hosts::NotifyField as F;
        let s = &self.snapshot;
        let toggle = |id: SettingId, element: &'static str, field: F, cx: &mut Context<Self>| {
            let panel = cx.entity().downgrade();
            Toggle::new(element, id.label(), s.notify.get(field))
                .on_toggle(move |on, _, cx| {
                    let _ = panel.update(cx, |_, cx| cx.emit(SettingsEvent::SetNotify(field, on)));
                })
                .into_any_element()
        };
        let mut out = Vec::new();
        let (description, control): (gpui::SharedString, AnyElement) = if s.notify_denied {
            (
                "Turned off for Silo in System Settings".into(),
                Self::emit_button("settings-notify-system", "Open System Settings", SettingsEvent::OpenNotificationSettings, cx),
            )
        } else {
            ("Programs that need you, finish or fail, from every host".into(), toggle(SettingId::Notifications, "settings-notify-on", F::On, cx))
        };
        out.push(self.group(
            Some("Notifications"),
            vec![
                Self::row(SettingId::Notifications.label(), Some(description), Some(control), false),
                Self::row(
                    SettingId::NotifyOnlyHidden.label(),
                    Some("Never for the block you are looking at".into()),
                    Some(toggle(SettingId::NotifyOnlyHidden, "settings-notify-hidden", F::OnlyHidden, cx)),
                    false,
                ),
            ],
        ));
        let reasons = [
            (SettingId::NotifyBlocked, "settings-notify-blocked", F::Blocked),
            (SettingId::NotifyDone, "settings-notify-done", F::Done),
            (SettingId::NotifyError, "settings-notify-error", F::Error),
            (SettingId::NotifyProgram, "settings-notify-program", F::Program),
        ];
        let rows = reasons.into_iter().map(|(id, element, field)| Self::row(id.label(), None, Some(toggle(id, element, field, cx)), false)).collect();
        out.push(self.group(Some("Notify when"), rows));
        out.push(Self::note("Each host can be turned off in Hosts."));
        out
    }

    /// Settings › Permissions: each macOS privacy permission Silo Server
    /// holds (what programs in Silo terminals get) with Grant, and
    /// Automation's page.
    fn permissions_page(&self, narrow: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let (list, problem) = match &self.snapshot.permissions {
            Some(Ok(list)) => (Some(list), None),
            Some(Err(problem)) => (None, Some(problem.clone())),
            None => (None, None),
        };
        let mut rows = Vec::new();
        for (index, id) in PermissionId::ALL.into_iter().enumerate() {
            let status = list.and_then(|l| l.get(id)).map(|p| p.status);
            let label = match (status, &problem) {
                (Some(PermissionStatus::Granted), _) => "Granted",
                (Some(PermissionStatus::Denied), _) => "Not granted",
                (Some(PermissionStatus::Unknown), _) => "Unknown",
                (None, Some(_)) => "Unknown",
                (None, None) => "Checking…",
            };
            let granted = status.is_some_and(PermissionStatus::granted);
            let control = div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().text_size(px(12.0)).text_color(rgb(if granted { theme::success() } else { theme::muted() })).child(label));
            let control = if granted {
                control
            } else {
                control.child(Self::emit_button(("settings-permission-grant", index), "Grant", SettingsEvent::GrantPermission(id), cx))
            };
            rows.push(Self::row(id.title(), Some(id.reason().into()), Some(control.into_any_element()), narrow));
        }
        out.push(self.group(Some("Silo Server"), rows));
        out.push(self.group(
            None,
            vec![Self::row(
                "Automation",
                Some("macOS asks the first time a program controls another app".into()),
                Some(Self::emit_button("settings-permission-automation", "Open Settings", SettingsEvent::OpenAutomationSettings, cx)),
                narrow,
            )],
        ));
        if let Some(problem) = problem {
            out.push(div().px(px(4.0)).text_size(px(11.5)).text_color(rgb(theme::warning())).child(problem).into_any_element());
        }
        out.push(Self::note(
            "Programs in Silo terminals get what Silo Server is granted. Programs already running may need restarting once granted.",
        ));
        out
    }

    /// A host row's notification switch.
    fn host_notify(&self, index: usize, host: &SettingsHost, cx: &mut Context<Self>) -> AnyElement {
        let panel = cx.entity().downgrade();
        let id = host.id.clone();
        let toggle = Toggle::new(("settings-host-notify", index), "Notifications", host.notify).on_toggle(move |on, _, cx| {
            let _ = panel.update(cx, |_, cx| cx.emit(SettingsEvent::SetHostNotify(id.clone(), on)));
        });
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child("Notify"))
            .child(toggle)
            .into_any_element()
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
            rows.push(Self::row(SettingId::PairDevice.label(), Some("Single use, valid 10 minutes".into()), Some(pair), narrow));
        }
        if let Some(ticket) = s.invite.clone() {
            let code = crate::qr::render(&ticket);
            let control = div().flex().items_center().gap(px(6.0)).child(div().max_w(px(180.0)).child(Self::value(ticket.clone()))).child(copy(
                "settings-copy-ticket",
                ticket,
                cx,
            ));
            rows.push(Self::row("Ticket", None, Some(control.into_any_element()), narrow));
            rows.extend(code.map(|qr| div().w_full().flex().justify_center().py(px(12.0)).child(qr).into_any_element()));
        }
        self.group(Some("Paired devices"), rows)
    }

    fn sidebar(&self, width: f32, searching: bool, cx: &mut Context<Self>) -> AnyElement {
        // Rounded like the card on its outer side; see `render`.
        let mut items = div().id("settings-sidebar-items").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().pb(px(8.0));
        for row in sidebar_rows(|section| self.shown(section) && (!searching || self.matches(section))) {
            let section = match row {
                SidebarRow::Label(group) => {
                    items = items.child(
                        div()
                            .mx(px(8.0))
                            .px(px(8.0))
                            .pt(px(10.0))
                            .pb(px(4.0))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::muted()))
                            .children(group.label()),
                    );
                    continue;
                }
                SidebarRow::Section(section) => section,
            };
            let index = Section::ALL.iter().position(|s| *s == section).unwrap_or_default();
            let selected = self.selected == section && !searching;
            items = items.child(
                div()
                    .id(("settings-section", index))
                    .h(px(36.0))
                    .mx(px(8.0))
                    .when(section.group() == Group::About, |s| s.mt(px(10.0)))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .continuous(7.0)
                    .cursor_pointer()
                    .when(selected, |s| s.bg(theme::row_fill()))
                    .when(!selected, |s| s.hover(|s| s.bg(theme::hover_fill())))
                    .child(
                        div().size(px(22.0)).flex_none().flex().items_center().justify_center().continuous(6.0).bg(rgb(section.tint())).child(icon(
                            section.icon(),
                            13.0,
                            0x00ff_ffff,
                        )),
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
            .bg(theme::group_fill())
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
                        .bg(theme::well_fill())
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
                .bg(theme::control_fill())
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
        self.scroll_to_reveal(window, cx);
        // The host clamps the card to the viewport; mirror that here.
        let panel_width = (f32::from(window.viewport_size().width) - 64.0).clamp(420.0, 880.0);
        let compact = panel_width < NARROW;
        let sidebar_width = if compact { 168.0 } else { 200.0 };
        let narrow = panel_width - sidebar_width < 480.0;
        let searching = !self.query.is_empty();

        let sections: Vec<_> = Section::ALL
            .into_iter()
            .filter(|section| if searching { self.matches(*section) } else { *section == self.selected && self.shown(*section) })
            .collect();
        let mut content =
            div().w_full().max_w(px(620.0)).relative().flex().flex_col().gap(px(14.0)).pb(px(28.0)).child(self.places.probe((None, "")));
        if let Some(message) = self.snapshot.message.clone() {
            content = content.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .continuous(8.0)
                    .bg(theme::hover_fill())
                    .text_size(px(12.0))
                    .text_color(rgb(theme::muted()))
                    .child(message),
            );
        }
        if sections.is_empty() {
            content = content.child(div().pt(px(40.0)).flex().justify_center().text_color(rgb(theme::muted())).child("No matching settings"));
        }
        for (i, section) in sections.into_iter().enumerate() {
            // The toolbar names a single page; search results get headings.
            if searching {
                let heading = div().when(i > 0, |h| h.pt(px(12.0))).text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(section.label());
                content = content.child(heading);
            }
            content = content.children(self.page(section, narrow, cx));
        }
        let title: gpui::SharedString = match self.selected {
            _ if searching => "Search Results".into(),
            section => section.label().into(),
        };
        let gutter = if compact { 20.0 } else { 28.0 };

        div()
            .id("settings-panel")
            .key_context("Settings")
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .flex()
            .overflow_hidden()
            .occlude()
            .font_family(theme::ui_font())
            .text_size(px(13.0))
            .text_color(rgb(theme::foreground()))
            // Frosted, the card's veil is the panel's.
            .when(theme::veils().is_none(), |el| el.bg(rgb(theme::background())))
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
                                    .hover(|s| s.bg(theme::hover_fill()))
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
    fn font_menu_opens_below_left_aligned_and_flips_near_the_bottom() {
        let viewport = gpui::size(px(800.0), px(600.0));
        let trigger = |y: f32| gpui::Bounds::new(gpui::point(px(400.0), px(y)), gpui::size(px(220.0), px(28.0)));
        let (anchor, at) = font_menu_place(trigger(100.0), viewport, px(300.0));
        assert_eq!((anchor, at), (Anchor::TopLeft, gpui::point(px(400.0), px(132.0))));
        let (anchor, at) = font_menu_place(trigger(500.0), viewport, px(300.0));
        assert_eq!((anchor, at), (Anchor::BottomLeft, gpui::point(px(400.0), px(496.0))));
    }

    #[test]
    fn defaults_only_offer_supported_layout_preferences() {
        let snapshot = SettingsSnapshot::default();
        assert_eq!(snapshot.density, Density::Comfortable);
        assert_eq!(snapshot.pane_headers, crate::pane_header::PaneHeaders::all(true));
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
        assert_eq!(Section::ALL.len(), 9);
        assert_eq!(Section::ALL[0], Section::Appearance, "Settings opens on Appearance");
        assert!(!Section::ALL.iter().any(|s| s.label() == "General"));
        assert!(!Section::ALL.iter().any(|s| ["Agents", "Models"].contains(&s.label())), "otto's settings are in the agent pane");
        for section in Section::ALL {
            assert!(!section.keywords().is_empty());
            assert!(crate::assets::has_icon(section.icon()), "missing icon {}", section.icon());
        }
        assert!(crate::assets::has_icon("settings-search"));
    }

    #[test]
    fn sections_are_grouped_by_what_they_set_with_about_last() {
        use Section as S;
        let order = [S::Appearance, S::Keyboard, S::Notifications, S::Terminal, S::Browser, S::Hosts, S::Server, S::Permissions, S::About];
        assert_eq!(Section::ALL, order);
        let groups: Vec<Group> = Section::ALL.iter().map(|s| s.group()).collect();
        use Group as G;
        assert_eq!(groups, [G::General, G::General, G::General, G::Terminal, G::Browser, G::Connections, G::Connections, G::Connections, G::About]);
        // Each group's pages are together, so each label shows once.
        let mut seen: Vec<Group> = Vec::new();
        for g in groups {
            if seen.last() != Some(&g) {
                assert!(!seen.contains(&g), "{g:?} split");
                seen.push(g);
            }
        }
        assert_eq!(Group::About.label(), None, "About stands alone");
        let labels: Vec<_> = [G::General, G::Terminal, G::Browser, G::Connections].iter().map(|g| g.label()).collect();
        assert_eq!(labels, [Some("General"), Some("Terminal"), Some("Browser"), Some("Connections")]);
    }

    #[test]
    fn the_sidebar_labels_only_groups_with_shown_pages() {
        use SidebarRow::{Label, Section as P};
        let all = sidebar_rows(|_| true);
        assert_eq!(all.iter().filter(|r| matches!(r, Label(_))).count(), 4);
        assert_eq!(all.iter().filter(|r| matches!(r, P(_))).count(), Section::ALL.len());
        assert_eq!(&all[..2], &[Label(Group::General), P(Section::Appearance)]);
        assert_eq!(all.last(), Some(&P(Section::About)), "About, unlabelled, at the bottom");
        // A search: only matching pages, under their own groups' labels.
        let snapshot = SettingsSnapshot::default();
        let shown = sidebar_rows(|s| section_matches(s, &snapshot, &["pair"]));
        assert_eq!(shown, [Label(Group::Connections), P(Section::Server)]);
        let shown = sidebar_rows(|s| matches!(s, Section::Keyboard | Section::Browser));
        assert_eq!(shown, [Label(Group::General), P(Section::Keyboard), Label(Group::Browser), P(Section::Browser)]);
        assert!(sidebar_rows(|_| false).is_empty());
        assert!(section_matches(Section::Browser, &snapshot, &["browser"]));
    }

    #[test]
    fn permissions_have_a_page_found_by_their_names() {
        let snapshot = SettingsSnapshot::default();
        for words in [&["screen", "recording"][..], &["full", "disk"], &["accessibility"], &["input", "monitoring"], &["automation"], &["privacy"]] {
            let shown: Vec<Section> = Section::ALL.into_iter().filter(|s| section_matches(*s, &snapshot, words)).collect();
            assert!(shown.contains(&Section::Permissions), "{words:?}: {shown:?}");
        }
        assert_eq!(Section::named("permissions"), Some(Section::Permissions));
        assert_eq!(Section::named("Server"), Some(Section::Server));
        assert_eq!(Section::named("nope"), None);
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

    #[test]
    fn places_tell_same_titled_groups_of_different_pages_apart() {
        let places = Places::default();
        {
            let mut p = places.0.borrow_mut();
            p.insert((None, ""), 100.0);
            p.insert((Some(Section::Appearance), "Colors"), 220.0);
            p.insert((Some(Section::Terminal), "Colors"), 480.0);
        }
        assert_eq!(places.offset(Section::Terminal, "Colors"), Some(380.0));
        assert_eq!(places.offset(Section::Appearance, "Colors"), Some(120.0));
        assert_eq!(places.offset(Section::Terminal, "Font"), None);
        // Each panel has its own.
        assert_eq!(Places::default().offset(Section::Terminal, "Colors"), None);
        places.clear();
        assert_eq!(places.offset(Section::Appearance, "Colors"), None);
    }

    /// The finder's settings are the panel's: searching the panel for a
    /// declared setting's title, label or a keyword shows its page.
    #[test]
    fn the_panel_search_finds_every_declared_setting() {
        let snapshot = SettingsSnapshot::default();
        for setting in crate::settings_table::SETTINGS {
            let keyword = setting.keywords.split_whitespace().next().unwrap_or_default();
            for text in [setting.title, setting.label, keyword] {
                let query = text.to_lowercase();
                let words: Vec<&str> = query.split_whitespace().collect();
                assert!(section_matches(setting.section, &snapshot, &words), "{text:?} misses {:?}", setting.section);
            }
        }
    }
}
