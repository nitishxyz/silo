//! The app theme, derived from Silo's own terminal preferences
//! ([`crate::terminal_prefs`]): terminal colors and font, with chrome
//! (titlebar, tabs, pill, popovers) derived from the terminal background and
//! foreground so the window reads as one surface.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};

use silo_proto::model::TerminalConfig;

use crate::{
    fonts,
    terminal_prefs::{Effective, TerminalPrefs},
};

/// The family chrome text renders in (titlebar, tabs, popovers, Settings,
/// palette, pane headers, sheets): the UI font preference resolved against
/// the installed families, [`fonts::UI_SYSTEM_FONT`] by default. Every UI
/// surface takes its family from here; only the terminal grid (and the
/// Settings preview of it) uses the terminal font.
#[must_use]
pub fn ui_font() -> gpui::SharedString {
    ui_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).family.clone()
}

/// The UI font preference and how it resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiFont {
    /// Exact stored family, or empty for Automatic (the system font).
    pub preferred: String,
    /// The family rendering.
    pub family: gpui::SharedString,
    /// The preferred family is not installed (using the system font).
    pub missing: bool,
}

impl Default for UiFont {
    fn default() -> Self {
        Self { preferred: String::new(), family: fonts::UI_SYSTEM_FONT.into(), missing: false }
    }
}

static UI: OnceLock<RwLock<UiFont>> = OnceLock::new();

fn ui_slot() -> &'static RwLock<UiFont> {
    UI.get_or_init(|| RwLock::new(UiFont::default()))
}

/// Resolve and install the UI font preference (at startup, on change, and
/// when Settings re-reads the installed fonts). Callers refresh the window.
pub fn apply_ui_font(preferred: &str, cx: &gpui::App) -> UiFont {
    let preferred = preferred.trim();
    let resolved = if preferred.is_empty() { fonts::resolve_ui("", &[]) } else { fonts::resolve_ui(preferred, &font_names(cx)) };
    if resolved.missing {
        tracing::warn!("{}", fonts::missing_problem(preferred, "the system font"));
    }
    let ui = UiFont { preferred: preferred.to_owned(), family: resolved.family.into(), missing: resolved.missing };
    *ui_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner) = ui.clone();
    ui
}

/// The current UI font state.
#[must_use]
pub fn ui_font_state() -> UiFont {
    ui_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

/// Superellipse exponent for every rounded surface: continuous ("squircle")
/// corners via the GPUI fork's `corner_smoothing`. Matches the desktop
/// platform's `CORNER_EXPONENT`. The renderer keeps circles circular and
/// draws capsules with circular tips and curvature-continuous joins.
pub const CORNER_SMOOTHING: f32 = 4.0;

/// Continuous corners. Smoothing is per element (not inherited); content
/// masks are rectangular unless an ancestor uses `rounded_clip` (a pane card
/// does), so otherwise a child whose fill reaches a parent's corner must
/// repeat its curve too.
pub trait Continuous: gpui::Styled + Sized {
    /// Continuous corners at `radius` on every corner.
    #[must_use]
    fn continuous(self, radius: f32) -> Self {
        self.rounded(gpui::px(radius)).corner_smoothing(CORNER_SMOOTHING)
    }

    /// Continuous corners with a radius per corner.
    #[must_use]
    fn continuous_corners(self, corners: crate::radius::CardCorners) -> Self {
        self.rounded_tl(gpui::px(corners.top_left))
            .rounded_tr(gpui::px(corners.top_right))
            .rounded_br(gpui::px(corners.bottom_right))
            .rounded_bl(gpui::px(corners.bottom_left))
            .corner_smoothing(CORNER_SMOOTHING)
    }

    /// Fully rounded ends (pills, circles); use instead of `rounded_full`.
    #[must_use]
    fn capsule(self) -> Self {
        self.rounded_full().corner_smoothing(CORNER_SMOOTHING)
    }
}

impl<T: gpui::Styled> Continuous for T {}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub prefs: TerminalPrefs,
    pub effective: Effective,
    /// The font the renderer actually uses (the preferred one if installed).
    pub font_in_use: String,
    /// Why the preferences on disk could not be used as written, and/or why
    /// the preferred font is unavailable; shown in Settings.
    pub load_problem: Option<String>,
    pub font_problem: Option<String>,
}

impl Theme {
    #[must_use]
    pub fn from_prefs(prefs: TerminalPrefs) -> Self {
        let effective = prefs.effective();
        // Until `apply_font` resolves it against the installed families.
        let font_in_use = if effective.font_family.is_empty() { fonts::FALLBACK_FONT.to_owned() } else { effective.font_family.clone() };
        Self { prefs, effective, font_in_use, load_problem: None, font_problem: None }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_prefs(TerminalPrefs::default())
    }
}

static THEME: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();

fn slot() -> &'static RwLock<Arc<Theme>> {
    THEME.get_or_init(|| RwLock::new(Arc::new(Theme::default())))
}

/// Install preferences as the current theme (at startup, on change and on
/// reload), with the problem found while loading them, if any.
pub fn install(prefs: TerminalPrefs, load_problem: Option<String>) {
    let mut theme = Theme::from_prefs(prefs);
    theme.load_problem = load_problem;
    *slot().write().unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(theme);
}

fn current() -> Arc<Theme> {
    slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

#[must_use]
pub fn prefs() -> TerminalPrefs {
    current().prefs.clone()
}

/// The problem found loading the stored preferences, if any.
#[must_use]
pub fn load_problem() -> Option<String> {
    current().load_problem.clone()
}

/// Problems with the stored preferences or the preferred font.
#[must_use]
pub fn problems() -> Vec<String> {
    let t = current();
    t.load_problem.iter().chain(t.font_problem.iter()).cloned().collect()
}

/// Install the preferred font into the renderer (case- and
/// space-insensitive match); Automatic and a missing family resolve per
/// [`fonts::resolve`]. Returns a problem when the preferred family is not
/// installed.
pub fn apply_font(cx: &gpui::App) -> Option<String> {
    let theme = current();
    let wanted = theme.effective.font_family.clone();
    let names = font_names(cx);
    let resolved = fonts::resolve(&wanted, &names);
    let problem = resolved.missing.then(|| fonts::missing_problem(&wanted, &resolved.family));
    if let Some(problem) = &problem {
        tracing::warn!("{problem}");
    }
    let font = silo_render::FontSettings { family: resolved.family, size: theme.effective.font_size, ..Default::default() };
    let in_use = font.family.clone();
    silo_render::set_terminal_font(font);
    let mut slot = slot().write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.font_in_use != in_use || slot.font_problem != problem {
        let mut theme = (**slot).clone();
        theme.font_in_use = in_use;
        theme.font_problem.clone_from(&problem);
        *slot = Arc::new(theme);
    }
    problem
}

/// Installed families, read from the system once and on [`refresh_fonts`],
/// and what probing each one found. A family is probed once per run, so
/// GPUI loads (and warns about) each face at most once.
#[derive(Default)]
struct FontCatalog {
    names: Vec<String>,
    probed: HashMap<String, Probe>,
}

/// `usable`: the text system loads the family itself rather than a
/// fallback; unusable families are never resolved again.
#[derive(Clone, Copy)]
struct Probe {
    usable: bool,
    monospace: bool,
}

static FONTS: OnceLock<RwLock<FontCatalog>> = OnceLock::new();

fn fonts_slot() -> &'static RwLock<FontCatalog> {
    FONTS.get_or_init(|| RwLock::new(FontCatalog::default()))
}

/// Re-read the installed families (fonts may be added or removed while
/// running); called when Settings opens and on reload. Only added
/// families are probed.
pub fn refresh_fonts(cx: &gpui::App) {
    let names = cx.text_system().all_font_names();
    fonts_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner).names = names;
}

/// Probe the installed families on a background thread at launch, so
/// Settings opens with its font lists ready.
pub fn warm_fonts(cx: &gpui::App) {
    let names = font_names(cx);
    let text = cx.text_system().clone();
    cx.background_executor().spawn(async move { probe(&text, &names) }).detach();
}

fn probe(text: &gpui::TextSystem, names: &[String]) {
    let todo = fonts::to_probe(&fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).probed, names);
    let size = gpui::px(16.0);
    let found: Vec<(String, Probe)> = todo
        .into_iter()
        .map(|name| {
            let probe = if name.starts_with('.') {
                Probe { usable: false, monospace: false }
            } else if fonts::NERD_FONT_CANDIDATES.iter().any(|c| fonts::key(c) == fonts::key(&name)) {
                Probe { usable: true, monospace: true }
            } else {
                // `resolve_font` substitutes a fallback face for unusable families.
                let id = text.resolve_font(&gpui::font(name.as_str()));
                let usable = text.get_font_for_id(id).is_some_and(|f| fonts::key(&f.family) == fonts::key(&name));
                let advances: Vec<f32> = ['i', 'M', 'W', '0']
                    .iter()
                    .filter(|_| usable)
                    .filter_map(|c| text.advance(id, size, *c).ok().map(|s| f32::from(s.width)))
                    .collect();
                Probe { usable, monospace: advances.len() == 4 && fonts::is_monospace(&advances) }
            };
            (name, probe)
        })
        .collect();
    fonts_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner).probed.extend(found);
}

/// Installed family names (system spelling).
#[must_use]
pub fn font_names(cx: &gpui::App) -> Vec<String> {
    if fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).names.is_empty() {
        refresh_fonts(cx);
    }
    fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).names.clone()
}

/// Installed families that pass `keep`, probing any not probed yet.
fn probed_fonts(cx: &gpui::App, keep: impl Fn(&str, Probe) -> bool) -> Vec<String> {
    let names = font_names(cx);
    probe(cx.text_system(), &names);
    let catalog = fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner);
    names.into_iter().filter(|name| catalog.probed.get(name).is_some_and(|p| keep(name, *p))).collect()
}

/// Installed monospaced families suitable for the terminal, in picker
/// order (supported Nerd Fonts first). A family is monospaced when `i`,
/// `M`, `W` and `0` advance equally.
#[must_use]
pub fn monospace_fonts(cx: &gpui::App) -> Vec<String> {
    fonts::picker_order(probed_fonts(cx, |_, p| p.monospace))
}

/// Installed text families for the UI font picker, alphabetically
/// (monospaced ones included; hidden, symbol and emoji families and ones
/// the text system cannot load are not).
#[must_use]
pub fn ui_fonts(cx: &gpui::App) -> Vec<String> {
    fonts::ui_picker_order(probed_fonts(cx, |name, p| p.usable && fonts::is_ui_candidate(name)))
}

// ---- color math ----

fn channels(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32, (c & 0xff) as f32]
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[must_use]
pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (a, b) = (channels(a), channels(b));
    let c = |i: usize| (a[i] + (b[i] - a[i]) * t).round().clamp(0.0, 255.0) as u32;
    (c(0) << 16) | (c(1) << 8) | c(2)
}

fn luminance(c: u32) -> f32 {
    let [r, g, b] = channels(c);
    (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
}

/// WCAG relative luminance (linearized sRGB), 0–1.
#[must_use]
pub fn relative_luminance(c: u32) -> f32 {
    let linear = |v: f32| {
        let v = v / 255.0;
        if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let [r, g, b] = channels(c);
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// WCAG contrast ratio, 1–21.
#[must_use]
pub fn contrast(a: u32, b: u32) -> f32 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

const DARK_INK: u32 = 0x001d_1d1f;
const LIGHT_INK: u32 = 0x00f5_f5f7;

/// Minimum contrast of text, and of icons and other non-text marks.
pub const TEXT_CONTRAST: f32 = 4.5;
pub const ICON_CONTRAST: f32 = 3.0;

/// Text on `bg`: the theme foreground on the theme background or wherever
/// it reads, else near-black or near-white, else pure black or white.
#[must_use]
pub fn ink_on(bg: u32) -> u32 {
    ink_for(bg, background(), foreground())
}

fn ink_for(bg: u32, theme_bg: u32, fg: u32) -> u32 {
    if bg == theme_bg || contrast(fg, bg) >= TEXT_CONTRAST {
        return fg;
    }
    let soft = if contrast(DARK_INK, bg) >= contrast(LIGHT_INK, bg) { DARK_INK } else { LIGHT_INK };
    if contrast(soft, bg) >= TEXT_CONTRAST {
        soft
    } else if contrast(0, bg) >= contrast(0x00ff_ffff, bg) {
        0
    } else {
        0x00ff_ffff
    }
}

/// An ink reading at 4.5:1 on every one of `bgs` (what a veil may look
/// like): the theme foreground if it does, else near-black or near-white,
/// else pure black or white.
#[must_use]
pub fn reads_on_all(bgs: &[u32]) -> Option<u32> {
    ink_all(bgs, foreground())
}

fn ink_all(bgs: &[u32], fg: u32) -> Option<u32> {
    [fg, DARK_INK, LIGHT_INK, 0, 0x00ff_ffff].into_iter().find(|&c| bgs.iter().all(|&b| contrast(c, b) >= TEXT_CONTRAST))
}

/// [`reads_on_all`], or the candidate whose worst contrast is best.
#[must_use]
pub fn ink_on_all(bgs: &[u32]) -> u32 {
    reads_on_all(bgs).unwrap_or_else(|| {
        let worst = |c: u32| bgs.iter().map(|&b| contrast(c, b)).fold(f32::MAX, f32::min);
        if worst(0) >= worst(0x00ff_ffff) { 0 } else { 0x00ff_ffff }
    })
}

/// The colour `from` of the way from `bg` toward `ink`, or the smallest
/// step past it that reaches `min` contrast (at most `ink` itself).
#[must_use]
pub fn toward_ink(bg: u32, ink: u32, from: f32, min: f32) -> u32 {
    reading(bg, ink, &[bg], from, min)
}

/// [`toward_ink`] from `base`, reaching `min` on every one of `bgs`.
#[must_use]
pub fn reading(base: u32, ink: u32, bgs: &[u32], from: f32, min: f32) -> u32 {
    let mut t = from;
    loop {
        let c = mix(base, ink, t);
        if t >= 1.0 || bgs.iter().all(|&b| contrast(c, b) >= min) {
            return c;
        }
        t = (t + 0.025).min(1.0);
    }
}

/// Secondary text on `bg`.
#[must_use]
pub fn muted_on(bg: u32) -> u32 {
    toward_ink(bg, ink_on(bg), 0.55, TEXT_CONTRAST)
}

/// Secondary icons on `bg`.
#[must_use]
pub fn icon_muted_on(bg: u32) -> u32 {
    toward_ink(bg, ink_on(bg), 0.55, ICON_CONTRAST)
}

/// The accent on `bg`, or its ink where the accent would not read.
#[must_use]
pub fn accent_on(bg: u32) -> u32 {
    if bg == background() || contrast(accent(), bg) >= ICON_CONTRAST { accent() } else { ink_on(bg) }
}

/// A light colour (dark ink reads on it).
#[must_use]
pub fn light(c: u32) -> bool {
    luminance(c) > 0.5
}

#[must_use]
pub fn is_light() -> bool {
    light(background())
}

// ---- palette accessors (chrome derives from bg/fg) ----

#[must_use]
pub fn background() -> u32 {
    current().effective.config.background
}
#[must_use]
pub fn foreground() -> u32 {
    current().effective.config.foreground
}
/// Window surface: titlebar, Comfortable gutters and pane headers all use
/// the terminal's own background, so the window reads as one surface.
#[must_use]
pub fn surface() -> u32 {
    background()
}
/// Neutral 1px outline of a Comfortable pane card filled with `fill`.
#[must_use]
pub fn card_outline_on(fill: u32) -> u32 {
    mix(fill, ink_on(fill), if light(fill) { 0.12 } else { 0.10 })
}
/// Fill of the current tab on `surface`: a subtle step above the rest.
#[must_use]
pub fn tab_selected_on(surface: u32) -> u32 {
    mix(surface, ink_on(surface), if light(surface) { 0.08 } else { 0.10 })
}
/// Overlay that dims inactive panes: the background at this alpha.
pub const INACTIVE_DIM: f32 = 0.38;
/// The same overlay over a web page: lighter, since pages are mostly bright.
pub const INACTIVE_DIM_PAGE: f32 = 0.14;

/// This frame's Frosted colours, set by the workspace; `None` when solid.
/// Surfaces over the app's own content (palette, menus, sheets, tooltips,
/// settings, floats) cannot be translucent: the window blur only blurs
/// what is behind the window. They take the material's look as one opaque
/// colour, so there is no alpha here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Veils {
    pub overlay: u32,
    /// Secondary text on it.
    pub muted: u32,
}

thread_local! {
    static VEILS: std::cell::Cell<Option<Veils>> = const { std::cell::Cell::new(None) };
}

pub fn set_veils(veils: Option<Veils>) {
    VEILS.set(veils);
}

#[must_use]
pub fn veils() -> Option<Veils> {
    VEILS.get()
}

/// Ink of the theme foreground at `amount` over a veil.
fn tint(amount: f32) -> gpui::Rgba {
    gpui::rgb(foreground()).opacity(amount)
}

/// Fill of surfaces over the app's own content: always opaque.
#[must_use]
pub fn overlay_fill() -> gpui::Rgba {
    gpui::rgb(overlay())
}
/// [`overlay_fill`]'s colour.
#[must_use]
pub fn overlay() -> u32 {
    veils().map_or_else(popover, |v| v.overlay)
}
/// Panels and grouped rows inside a floating surface (never a second veil).
#[must_use]
pub fn group_fill() -> gpui::Rgba {
    if veils().is_some() { tint(0.04) } else { gpui::rgb(popover()) }
}
/// Buttons, switch tracks, segmented controls.
#[must_use]
pub fn control_fill() -> gpui::Rgba {
    if veils().is_some() { tint(0.10) } else { gpui::rgb(tab_active()) }
}
/// Selected rows, pressed controls.
#[must_use]
pub fn row_fill() -> gpui::Rgba {
    if veils().is_some() { tint(0.14) } else { gpui::rgb(row_active()) }
}
/// Hovered rows.
#[must_use]
pub fn hover_fill() -> gpui::Rgba {
    if veils().is_some() { tint(0.06) } else { gpui::rgb(tab_hover()) }
}
/// Text input wells.
#[must_use]
pub fn well_fill() -> gpui::Rgba {
    if veils().is_some() { tint(0.06) } else { gpui::rgb(background()) }
}

#[must_use]
pub fn tab_hover() -> u32 {
    mix(background(), foreground(), 0.06)
}
#[must_use]
pub fn tab_active() -> u32 {
    mix(background(), foreground(), 0.11)
}
#[must_use]
pub fn popover() -> u32 {
    mix(background(), foreground(), if is_light() { 0.03 } else { 0.05 })
}
#[must_use]
pub fn row_active() -> u32 {
    mix(background(), foreground(), 0.15)
}
#[must_use]
pub fn border() -> u32 {
    mix(background(), foreground(), 0.13)
}
/// Secondary text; frosted, it reads on the overlay veil.
#[must_use]
pub fn muted() -> u32 {
    veils().map_or_else(|| mix(background(), foreground(), 0.55), |v| v.muted)
}
/// The theme's bright blue.
#[must_use]
pub fn accent() -> u32 {
    current().effective.config.palette[12]
}
/// Lighter top of the accent gradient on toggles and segmented thumbs.
#[must_use]
pub fn accent_highlight() -> u32 {
    mix(accent(), 0x00ff_ffff, 0.22)
}
#[must_use]
pub fn danger() -> u32 {
    current().effective.config.palette[1]
}
/// The theme's green.
#[must_use]
pub fn success() -> u32 {
    current().effective.config.palette[2]
}
/// The theme's yellow.
#[must_use]
pub fn warning() -> u32 {
    current().effective.config.palette[3]
}
#[must_use]
pub fn selection() -> u32 {
    current().effective.selection
}
/// Text on an accent-colored button.
#[must_use]
pub fn on_accent() -> u32 {
    if luminance(accent()) > 0.6 { 0x0000_0000 } else { 0x00ff_ffff }
}

/// The Update pill's fill: a saturated blue close to macOS systemBlue in
/// every theme (the accent may be pastel), deep enough for white text
/// ([`TEXT_CONTRAST`]); a touch brighter on dark themes.
#[must_use]
pub fn update_blue() -> u32 {
    update_blue_for(is_light())
}

fn update_blue_for(light: bool) -> u32 {
    if light { 0x0000_64d2 } else { 0x000a_6ce8 }
}

// ---- syntax colours (code in the Files block and agent code blocks) ----

/// Where a syntax colour comes from in the current terminal palette, so code
/// takes every built-in theme's own colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SyntaxInk {
    /// The theme foreground.
    Foreground,
    /// An ANSI palette slot, 0–15.
    Ansi(u8),
    /// Halfway between two ANSI slots: orange is red and yellow.
    Blend(u8, u8),
    /// The foreground faded this far toward the background.
    Faded(f32),
}

/// A syntax table entry: a colour plus font style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxStyle {
    pub ink: SyntaxInk,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl SyntaxStyle {
    const fn ink(ink: SyntaxInk) -> Self {
        Self { ink, bold: false, italic: false, underline: false }
    }
    const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }
    const fn italic(self) -> Self {
        Self { italic: true, ..self }
    }
    const fn underline(self) -> Self {
        Self { underline: true, ..self }
    }
}

const RED: SyntaxInk = SyntaxInk::Ansi(1);
const GREEN: SyntaxInk = SyntaxInk::Ansi(2);
const YELLOW: SyntaxInk = SyntaxInk::Ansi(3);
const BLUE: SyntaxInk = SyntaxInk::Ansi(4);
const MAGENTA: SyntaxInk = SyntaxInk::Ansi(5);
const CYAN: SyntaxInk = SyntaxInk::Ansi(6);
const ORANGE: SyntaxInk = SyntaxInk::Blend(1, 3);
const FG: SyntaxInk = SyntaxInk::Foreground;
const COMMENT: SyntaxInk = SyntaxInk::Faded(0.5);
const SUBTLE: SyntaxInk = SyntaxInk::Faded(0.35);
const PUNCTUATION: SyntaxInk = SyntaxInk::Faded(0.12);

/// The syntax table, keyed by tree-sitter capture name. A capture takes the
/// entry of its longest dotted prefix (`function.method` → `function`).
/// Keys and colours follow Zed's One Dark / One Light, each mapped to the
/// nearest terminal palette slot; the `text.*`, `escape`, `delimiter`,
/// `*.builtin` and `comment.documentation` keys are the names the upstream
/// grammars' queries use where Zed's differ.
pub const SYNTAX: &[(&str, SyntaxStyle)] = &[
    ("attribute", SyntaxStyle::ink(BLUE)),
    ("boolean", SyntaxStyle::ink(ORANGE)),
    ("comment", SyntaxStyle::ink(COMMENT)),
    ("comment.doc", SyntaxStyle::ink(SUBTLE)),
    ("comment.documentation", SyntaxStyle::ink(SUBTLE)),
    ("constant", SyntaxStyle::ink(YELLOW)),
    ("constant.builtin", SyntaxStyle::ink(ORANGE)),
    ("constructor", SyntaxStyle::ink(BLUE)),
    ("delimiter", SyntaxStyle::ink(PUNCTUATION)),
    ("diff.minus", SyntaxStyle::ink(RED)),
    ("diff.plus", SyntaxStyle::ink(GREEN)),
    ("embedded", SyntaxStyle::ink(FG)),
    ("emphasis", SyntaxStyle::ink(BLUE).italic()),
    ("emphasis.strong", SyntaxStyle::ink(ORANGE).bold()),
    ("enum", SyntaxStyle::ink(CYAN)),
    ("escape", SyntaxStyle::ink(SUBTLE)),
    ("function", SyntaxStyle::ink(BLUE)),
    ("keyword", SyntaxStyle::ink(MAGENTA)),
    ("label", SyntaxStyle::ink(BLUE)),
    ("link_text", SyntaxStyle::ink(BLUE).italic()),
    ("link_uri", SyntaxStyle::ink(CYAN).underline()),
    ("number", SyntaxStyle::ink(ORANGE)),
    ("operator", SyntaxStyle::ink(CYAN)),
    ("preproc", SyntaxStyle::ink(MAGENTA)),
    ("property", SyntaxStyle::ink(RED)),
    ("punctuation", SyntaxStyle::ink(PUNCTUATION)),
    ("punctuation.list_marker", SyntaxStyle::ink(RED)),
    ("punctuation.special", SyntaxStyle::ink(RED)),
    ("selector", SyntaxStyle::ink(YELLOW)),
    ("selector.pseudo", SyntaxStyle::ink(BLUE)),
    ("string", SyntaxStyle::ink(GREEN)),
    ("string.escape", SyntaxStyle::ink(SUBTLE)),
    ("string.regex", SyntaxStyle::ink(ORANGE)),
    ("string.special", SyntaxStyle::ink(ORANGE)),
    ("tag", SyntaxStyle::ink(BLUE)),
    ("text.emphasis", SyntaxStyle::ink(FG).italic()),
    ("text.literal", SyntaxStyle::ink(GREEN)),
    ("text.reference", SyntaxStyle::ink(BLUE)),
    ("text.strong", SyntaxStyle::ink(FG).bold()),
    ("text.title", SyntaxStyle::ink(RED).bold()),
    ("text.uri", SyntaxStyle::ink(CYAN).underline()),
    ("title", SyntaxStyle::ink(RED).bold()),
    ("type", SyntaxStyle::ink(CYAN)),
    ("variable", SyntaxStyle::ink(FG)),
    ("variable.builtin", SyntaxStyle::ink(ORANGE)),
    ("variable.parameter", SyntaxStyle::ink(RED)),
    ("variable.special", SyntaxStyle::ink(ORANGE)),
    ("variant", SyntaxStyle::ink(BLUE)),
];

/// Minimum contrast of syntax colours on the code's background: palette
/// colours that fall short (yellow on a light theme) step toward the ink.
pub const SYNTAX_CONTRAST: f32 = 3.0;

/// The [`SYNTAX`] entry named exactly `key`.
#[must_use]
pub fn syntax_style(key: &str) -> Option<SyntaxStyle> {
    SYNTAX.iter().find(|(k, _)| *k == key).map(|&(_, s)| s)
}

/// `ink` in the current theme, reading on `bg` (the code's background).
#[must_use]
pub fn syntax_color(ink: SyntaxInk, bg: u32) -> u32 {
    let config = terminal_config();
    syntax_color_in(&config.palette, config.foreground, config.background, ink, bg)
}

/// [`syntax_color`] for a given palette, foreground and theme background.
#[must_use]
pub fn syntax_color_in(palette: &[u32; 16], fg: u32, theme_bg: u32, ink: SyntaxInk, bg: u32) -> u32 {
    let slot = |i: u8| palette[usize::from(i) % 16];
    let base = match ink {
        SyntaxInk::Foreground => fg,
        SyntaxInk::Ansi(i) => slot(i),
        SyntaxInk::Blend(a, b) => mix(slot(a), slot(b), 0.5),
        SyntaxInk::Faded(t) => mix(fg, theme_bg, t),
    };
    reading(base, ink_for(bg, theme_bg, fg), &[bg], 0.0, SYNTAX_CONTRAST)
}

/// This app's desired terminal configuration, reported to the server for
/// every attached block.
#[must_use]
pub fn terminal_config() -> TerminalConfig {
    current().effective.config
}

/// Theme display name and the font actually in use.
#[must_use]
pub fn summary() -> (&'static str, String, f32) {
    let t = current();
    (t.effective.theme_name, t.font_in_use.clone(), t.effective.font_size)
}

#[cfg(test)]
mod tests {
    use gpui::{Styled, px};

    use super::{CORNER_SMOOTHING, Continuous};

    fn style(mut element: gpui::Div) -> gpui::StyleRefinement {
        element.style().clone()
    }

    #[test]
    fn update_blue_is_saturated_and_reads_white() {
        for light in [true, false] {
            let blue = super::update_blue_for(light);
            let [r, g, b] = super::channels(blue);
            assert!(b > 200.0 && r < 20.0 && g < 120.0, "{blue:06x} is a saturated blue");
            assert!(super::contrast(0x00ff_ffff, blue) >= super::TEXT_CONTRAST, "{blue:06x}");
        }
    }

    #[test]
    fn overlays_are_opaque_when_frosted() {
        super::set_veils(Some(super::Veils { overlay: 0x0012_3456, muted: 0x0080_8080 }));
        let fill = super::overlay_fill();
        super::set_veils(None);
        assert!((fill.a - 1.0).abs() < f32::EPSILON);
        assert_eq!(fill, gpui::rgb(0x0012_3456));
    }

    #[test]
    fn contrast_follows_relative_luminance() {
        assert!((super::relative_luminance(0x00ff_ffff) - 1.0).abs() < 1e-4);
        assert!(super::relative_luminance(0).abs() < 1e-6);
        assert!((super::contrast(0, 0x00ff_ffff) - 21.0).abs() < 1e-3);
        assert!((super::contrast(0x0012_3456, 0x0012_3456) - 1.0).abs() < 1e-6);
        // sRGB mid-grey is darker than half: #777 on white is just under 4.5:1.
        assert!(super::contrast(0x0077_7777, 0x00ff_ffff) < 4.5);
        assert!(super::contrast(0x0076_7676, 0x00ff_ffff) >= 4.5);
    }

    #[test]
    fn ink_keeps_the_theme_foreground_where_it_reads() {
        let (bg, fg) = (0x0028_2c34, 0x00ab_b2bf);
        assert_eq!(super::ink_for(bg, bg, fg), fg, "the theme pair is never second-guessed");
        assert_eq!(super::ink_for(0x001e_1b4b, bg, fg), fg, "a dark backdrop keeps it");
        assert_eq!(super::ink_for(0x00f0_f0e8, bg, fg), super::DARK_INK, "light backdrop: dark ink");
        assert_eq!(super::ink_for(0x00fa_fafa, 0x00ff_ffff, 0x0090_9090), super::DARK_INK);
        assert_eq!(super::ink_for(0x0010_1010, 0x00ff_ffff, 0x0030_3030), super::LIGHT_INK, "dark backdrop of a light theme");
        assert_eq!(super::ink_for(0x0080_8080, bg, fg), 0, "mid grey: near-black is short of 4.5:1, so pure black");
    }

    /// Backdrops across the range, including the hard middle.
    const BACKDROPS: [u32; 14] = [
        0x00ff_ffff,
        0x00c0_c0c0,
        0x0080_8080,
        0x0076_7676,
        0x0060_6060,
        0x003a_6ea5,
        0x0000_0000,
        0x00ff_0000,
        0x0000_ff00,
        0x0000_00ff,
        0x00ff_ff00,
        0x00ff_00ff,
        0x0000_ffff,
        0x00f4_ecd8,
    ];

    #[test]
    fn ink_and_muted_reach_their_contrast_on_any_backdrop() {
        use super::{ICON_CONTRAST, TEXT_CONTRAST, contrast, ink_for, toward_ink};
        for (theme_bg, fg) in [(0x0028_2c34, 0x00ab_b2bf), (0x00fa_fafa, 0x0038_3a42)] {
            for bg in BACKDROPS {
                let ink = ink_for(bg, theme_bg, fg);
                assert!(contrast(ink, bg) >= TEXT_CONTRAST, "ink {ink:06x} on {bg:06x}: {}", contrast(ink, bg));
                for (from, min) in [(0.55, TEXT_CONTRAST), (0.7, TEXT_CONTRAST), (0.55, ICON_CONTRAST)] {
                    let c = toward_ink(bg, ink, from, min);
                    assert!(contrast(c, bg) >= min, "{c:06x} on {bg:06x}: {} < {min}", contrast(c, bg));
                }
                let icon = toward_ink(bg, ink, 0.55, ICON_CONTRAST);
                let text = toward_ink(bg, ink, 0.55, TEXT_CONTRAST);
                assert!(contrast(icon, bg) <= contrast(text, bg) + 1e-3, "icons stay at most as strong as text");
            }
        }
    }

    #[test]
    fn muted_keeps_its_step_where_it_already_reads() {
        let (bg, ink) = (0x0000_0000, 0x00ff_ffff);
        assert_eq!(super::toward_ink(bg, ink, 0.55, super::TEXT_CONTRAST), super::mix(bg, ink, 0.55));
        let (bg, ink) = (0x0076_7676, 0x00ff_ffff);
        assert_eq!(super::toward_ink(bg, ink, 0.55, super::TEXT_CONTRAST), ink, "only the ink itself reads on #767676");
    }

    #[test]
    fn mix_blends_channels() {
        assert_eq!(super::mix(0, 0x00ff_ffff, 0.0), 0);
        assert_eq!(super::mix(0, 0x00ff_ffff, 1.0), 0x00ff_ffff);
        assert_eq!(super::mix(0x0000_00ff, 0x00ff_0000, 0.5), 0x0080_0080);
    }

    #[test]
    fn rounded_surfaces_are_continuous() {
        const { assert!(CORNER_SMOOTHING > 2.0, "2.0 or below renders circular corners") };
        let card = style(gpui::div().continuous(12.0));
        assert_eq!(card.corner_smoothing, Some(CORNER_SMOOTHING));
        assert_eq!(card.corner_radii.top_left, Some(px(12.0).into()));
        assert_eq!(card.corner_radii.bottom_right, Some(px(12.0).into()));

        let pill = style(gpui::div().capsule());
        assert_eq!(pill.corner_smoothing, Some(CORNER_SMOOTHING));
        assert_eq!(pill.corner_radii, style(gpui::div().rounded_full()).corner_radii);
    }

    #[test]
    fn chrome_takes_its_family_from_the_ui_font() {
        assert_eq!(super::ui_font_state(), super::UiFont::default());
        assert_eq!(super::ui_font(), crate::fonts::UI_SYSTEM_FONT);
        // No chrome module hard-codes a family; all go through `ui_font()`.
        let chrome = [
            ("titlebar.rs", include_str!("titlebar.rs")),
            ("switcher.rs", include_str!("switcher.rs")),
            ("palette.rs", include_str!("palette.rs")),
            ("layout.rs", include_str!("layout.rs")),
            ("tooltip.rs", include_str!("tooltip.rs")),
            ("appearance.rs", include_str!("appearance.rs")),
            ("add_host.rs", include_str!("add_host.rs")),
            ("settings_panel.rs", include_str!("settings_panel.rs")),
            ("workspace.rs", include_str!("workspace.rs")),
        ];
        for (name, source) in chrome {
            assert!(!source.contains(concat!(".font_family", "(\"")), "{name} hard-codes a font family");
            assert!(!source.contains(concat!(".SystemUI", "Font")), "{name} names the system font directly");
        }
    }
}
