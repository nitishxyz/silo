//! The app theme, derived from Silo's own terminal preferences
//! ([`crate::terminal_prefs`]): terminal colors and font, with chrome
//! (titlebar, tabs, pill, popovers) derived from the terminal background and
//! foreground so the window reads as one surface.

use std::sync::{Arc, OnceLock, RwLock};

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
    let resolved = if preferred.is_empty() {
        fonts::resolve_ui("", &[])
    } else {
        fonts::resolve_ui(preferred, &font_names(cx))
    };
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

/// Continuous corners. Smoothing is per element (not inherited), and content
/// masks stay rectangular, so a child whose fill reaches a parent's corner
/// must repeat the parent's curve with these helpers too.
pub trait Continuous: gpui::Styled + Sized {
    /// Continuous corners at `radius` on every corner.
    #[must_use]
    fn continuous(self, radius: f32) -> Self {
        self.rounded(gpui::px(radius)).corner_smoothing(CORNER_SMOOTHING)
    }

    /// Continuous corners on the left side only.
    #[must_use]
    fn continuous_l(self, radius: f32) -> Self {
        self.rounded_l(gpui::px(radius)).corner_smoothing(CORNER_SMOOTHING)
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
        let font_in_use =
            if effective.font_family.is_empty() { fonts::FALLBACK_FONT.to_owned() } else { effective.font_family.clone() };
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
    let font = silo_render::FontSettings {
        family: resolved.family,
        size: theme.effective.font_size,
        ..Default::default()
    };
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

/// Installed families, read from the system once and on [`refresh_fonts`];
/// the picker lists (monospaced for the terminal, text faces for the UI)
/// are computed lazily, once per catalog.
#[derive(Default)]
struct FontCatalog {
    names: Vec<String>,
    monospace: Option<Vec<String>>,
    ui: Option<Vec<String>>,
}

static FONTS: OnceLock<RwLock<FontCatalog>> = OnceLock::new();

fn fonts_slot() -> &'static RwLock<FontCatalog> {
    FONTS.get_or_init(|| RwLock::new(FontCatalog::default()))
}

/// Re-read the installed families (fonts may be added or removed while
/// running); called when Settings opens and on reload.
pub fn refresh_fonts(cx: &gpui::App) {
    let names = cx.text_system().all_font_names();
    let mut catalog = fonts_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if catalog.names != names {
        *catalog = FontCatalog { names, monospace: None, ui: None };
    }
}

/// Installed family names (system spelling).
#[must_use]
pub fn font_names(cx: &gpui::App) -> Vec<String> {
    if fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).names.is_empty() {
        refresh_fonts(cx);
    }
    fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).names.clone()
}

/// Installed monospaced families suitable for the terminal, in picker
/// order (supported Nerd Fonts first). Measured once per catalog: a family
/// is monospaced when `i`, `M`, `W` and `0` advance equally.
#[must_use]
pub fn monospace_fonts(cx: &gpui::App) -> Vec<String> {
    let names = font_names(cx);
    if let Some(mono) = &fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).monospace {
        return mono.clone();
    }
    let text = cx.text_system();
    let size = gpui::px(16.0);
    let mono: Vec<String> = names
        .iter()
        .filter(|name| !name.starts_with('.'))
        .filter(|name| {
            if fonts::NERD_FONT_CANDIDATES.iter().any(|c| fonts::key(c) == fonts::key(name)) {
                return true;
            }
            let id = text.resolve_font(&gpui::font(name.as_str()));
            // `resolve_font` substitutes a fallback face for unusable families.
            if text.get_font_for_id(id).is_none_or(|f| fonts::key(&f.family) != fonts::key(name)) {
                return false;
            }
            let advances: Vec<f32> =
                ['i', 'M', 'W', '0'].iter().filter_map(|c| text.advance(id, size, *c).ok().map(|s| f32::from(s.width))).collect();
            advances.len() == 4 && fonts::is_monospace(&advances)
        })
        .cloned()
        .collect();
    let mono = fonts::picker_order(mono);
    let mut catalog = fonts_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if catalog.names == names {
        catalog.monospace = Some(mono.clone());
    }
    mono
}

/// Installed text families for the UI font picker, alphabetically
/// (monospaced ones included; hidden, symbol and emoji families and ones
/// the text system cannot load are not). Computed once per catalog.
#[must_use]
pub fn ui_fonts(cx: &gpui::App) -> Vec<String> {
    let names = font_names(cx);
    if let Some(ui) = &fonts_slot().read().unwrap_or_else(std::sync::PoisonError::into_inner).ui {
        return ui.clone();
    }
    let text = cx.text_system();
    let usable: Vec<String> = names
        .iter()
        .filter(|name| fonts::is_ui_candidate(name))
        .filter(|name| {
            // `resolve_font` substitutes a fallback face for unusable families.
            let id = text.resolve_font(&gpui::font(name.as_str()));
            text.get_font_for_id(id).is_some_and(|f| fonts::key(&f.family) == fonts::key(name))
        })
        .cloned()
        .collect();
    let ui = fonts::ui_picker_order(usable);
    let mut catalog = fonts_slot().write().unwrap_or_else(std::sync::PoisonError::into_inner);
    if catalog.names == names {
        catalog.ui = Some(ui.clone());
    }
    ui
}

// ---- color math ----

fn channels(c: u32) -> [f32; 3] {
    [((c >> 16) & 0xff) as f32, ((c >> 8) & 0xff) as f32, (c & 0xff) as f32]
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (a, b) = (channels(a), channels(b));
    let c: Vec<u32> = (0..3).map(|i| (a[i] + (b[i] - a[i]) * t).round().clamp(0.0, 255.0) as u32).collect();
    (c[0] << 16) | (c[1] << 8) | c[2]
}

fn luminance(c: u32) -> f32 {
    let [r, g, b] = channels(c);
    (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
}

#[must_use]
pub fn is_light() -> bool {
    luminance(background()) > 0.5
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
/// Titlebar: the same surface as the terminal.
#[must_use]
pub fn chrome() -> u32 {
    background()
}
/// Window surface: titlebar, Comfortable gutters and pane headers all use
/// the terminal's own background, so the window reads as one surface.
#[must_use]
pub fn surface() -> u32 {
    background()
}
/// Neutral 1px outline of a Comfortable pane card.
#[must_use]
pub fn card_outline() -> u32 {
    mix(surface(), foreground(), if is_light() { 0.12 } else { 0.10 })
}
/// Fill of the current tab on `surface`: a subtle step above the rest.
#[must_use]
pub fn tab_selected_on(surface: u32) -> u32 {
    mix(surface, foreground(), if is_light() { 0.08 } else { 0.10 })
}
/// Hover fill on `surface`: only slightly lighter (darker on light themes)
/// than the surface itself.
#[must_use]
pub fn tab_hover_on(surface: u32) -> u32 {
    mix(surface, foreground(), if is_light() { 0.06 } else { 0.07 })
}
/// Overlay that dims inactive panes: the background at this alpha.
pub const INACTIVE_DIM: f32 = 0.38;
/// The same overlay over a web page: lighter, since pages are mostly bright.
pub const INACTIVE_DIM_PAGE: f32 = 0.14;

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
#[must_use]
pub fn muted() -> u32 {
    mix(background(), foreground(), 0.55)
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
#[must_use]
pub fn selection() -> u32 {
    current().effective.selection
}
/// Text on an accent-colored button.
#[must_use]
pub fn on_accent() -> u32 {
    if luminance(accent()) > 0.6 { 0x0000_0000 } else { 0x00ff_ffff }
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
    fn rounded_surfaces_are_continuous() {
        const { assert!(CORNER_SMOOTHING > 2.0, "2.0 or below renders circular corners") };
        let card = style(gpui::div().continuous(12.0));
        assert_eq!(card.corner_smoothing, Some(CORNER_SMOOTHING));
        assert_eq!(card.corner_radii.top_left, Some(px(12.0).into()));
        assert_eq!(card.corner_radii.bottom_right, Some(px(12.0).into()));

        let side = style(gpui::div().continuous_l(11.0));
        assert_eq!(side.corner_smoothing, Some(CORNER_SMOOTHING));
        assert_eq!(side.corner_radii.bottom_left, Some(px(11.0).into()));
        assert_eq!(side.corner_radii.top_right, None);

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
