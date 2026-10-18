//! Font policy: which installed family the terminal renderer uses, and
//! which one the app chrome (UI font) uses.
//!
//! The preference is either an exact family name or empty for "Automatic".
//! Automatic picks the first installed [`NERD_FONT_CANDIDATES`] entry so
//! prompt icons and Powerline glyphs render, and falls back to
//! [`FALLBACK_FONT`] (always present on macOS) when none is installed. A
//! stored family that is no longer installed is flagged and falls back the
//! same way. Nothing is downloaded or bundled.
//!
//! The UI font works the same way, except that Automatic (and a missing
//! family) is the macOS system UI font, [`UI_SYSTEM_FONT`].

/// Supported Nerd Font families, in order of preference. "Mono" variants
/// first: their icons are one cell wide, so the grid stays stable.
pub const NERD_FONT_CANDIDATES: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "Berkeley Mono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "FiraCode Nerd Font",
    "MesloLGS Nerd Font Mono",
    "MesloLGS Nerd Font",
    "MesloLGS NF",
    "Hack Nerd Font Mono",
    "Hack Nerd Font",
    "CaskaydiaCove Nerd Font Mono",
    "CaskaydiaMono Nerd Font Mono",
    "SauceCodePro Nerd Font Mono",
    "IosevkaTerm Nerd Font Mono",
    "UbuntuMono Nerd Font Mono",
];

/// Used when no supported Nerd Font is installed.
pub const FALLBACK_FONT: &str = "Menlo";

/// GPUI's name for the macOS system UI font: the UI font's Automatic.
pub const UI_SYSTEM_FONT: &str = ".SystemUIFont";

/// Installed families that draw symbols or emoji instead of text; never
/// offered as the UI font.
const NON_TEXT_FAMILIES: &[&str] = &[
    "Apple Color Emoji",
    "Apple Symbols",
    "Bodoni Ornaments",
    "LastResort",
    "Noto Color Emoji",
    "Symbol",
    "Webdings",
    "Wingdings",
    "Wingdings 2",
    "Wingdings 3",
    "Zapf Dingbats",
];

/// Case- and space-insensitive identity of a family name.
#[must_use]
pub fn key(name: &str) -> String {
    name.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase()
}

/// The installed spelling of `wanted`, if installed.
#[must_use]
pub fn find<'a>(wanted: &str, installed: &'a [String]) -> Option<&'a String> {
    let wanted = key(wanted);
    installed.iter().find(|have| key(have) == wanted)
}

/// Whether a family looks like a patched Nerd Font.
#[must_use]
pub fn is_nerd(name: &str) -> bool {
    let k = key(name);
    k.contains("nerdfont") || k.ends_with("nf") || k.ends_with("nfm") || NERD_FONT_CANDIDATES.iter().any(|c| key(c) == k)
}

/// What Automatic resolves to on this machine: the first installed Nerd
/// Font candidate (installed spelling), else the fallback.
#[must_use]
pub fn automatic(installed: &[String]) -> Automatic {
    NERD_FONT_CANDIDATES
        .iter()
        .find_map(|c| find(c, installed))
        .map_or_else(
            || Automatic { family: find(FALLBACK_FONT, installed).cloned().unwrap_or_else(|| FALLBACK_FONT.into()), nerd: false },
            |family| Automatic { family: family.clone(), nerd: true },
        )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Automatic {
    pub family: String,
    /// False when no supported Nerd Font is installed (using the fallback).
    pub nerd: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The family the renderer uses (installed spelling).
    pub family: String,
    /// The preference is a family that is not installed.
    pub missing: bool,
}

/// Resolve a preference (`""` = Automatic) against the installed families.
#[must_use]
pub fn resolve(preferred: &str, installed: &[String]) -> Resolved {
    let preferred = preferred.trim();
    if !preferred.is_empty()
        && let Some(family) = find(preferred, installed)
    {
        return Resolved { family: family.clone(), missing: false };
    }
    Resolved { family: automatic(installed).family, missing: !preferred.is_empty() }
}

/// User-facing problem for a missing preferred family.
#[must_use]
pub fn missing_problem(preferred: &str, using: &str) -> String {
    format!("Font \u{201c}{preferred}\u{201d} is not installed; using {using}")
}

/// Resolve a UI font preference (`""` = Automatic, the system UI font). A
/// family that is not installed falls back to the system font, flagged.
#[must_use]
pub fn resolve_ui(preferred: &str, installed: &[String]) -> Resolved {
    let preferred = preferred.trim();
    if preferred.is_empty() {
        return Resolved { family: UI_SYSTEM_FONT.into(), missing: false };
    }
    match find(preferred, installed) {
        Some(family) => Resolved { family: family.clone(), missing: false },
        None => Resolved { family: UI_SYSTEM_FONT.into(), missing: true },
    }
}

/// Whether a family is a text face usable for the UI (not hidden, not a
/// symbol or emoji font).
#[must_use]
pub fn is_ui_candidate(name: &str) -> bool {
    let k = key(name);
    !name.starts_with('.') && !k.is_empty() && !NON_TEXT_FAMILIES.iter().any(|f| key(f) == k)
}

/// UI font picker order: text families alphabetically, duplicates dropped.
#[must_use]
pub fn ui_picker_order(mut families: Vec<String>) -> Vec<String> {
    families.retain(|f| is_ui_candidate(f));
    families.sort_by_key(|f| key(f));
    families.dedup_by(|a, b| key(a) == key(b));
    families
}

/// Advances of `i`, `M`, `W` and `0` in one face: equal for monospaced.
#[must_use]
pub fn is_monospace(advances: &[f32]) -> bool {
    let Some(first) = advances.first() else { return false };
    *first > 0.0 && advances.iter().all(|a| (a - first).abs() <= 0.01 * first.max(1.0))
}

/// Picker order: supported Nerd Fonts (candidate order), other Nerd Fonts,
/// then the rest alphabetically; hidden system families dropped.
#[must_use]
pub fn picker_order(mut families: Vec<String>) -> Vec<String> {
    families.retain(|f| !f.starts_with('.') && !f.trim().is_empty());
    families.sort_by_key(|f| key(f));
    families.dedup_by(|a, b| key(a) == key(b));
    let rank = |f: &String| {
        let k = key(f);
        match NERD_FONT_CANDIDATES.iter().position(|c| key(c) == k) {
            Some(i) => i,
            None if is_nerd(f) => NERD_FONT_CANDIDATES.len(),
            None => NERD_FONT_CANDIDATES.len() + 1,
        }
    };
    families.sort_by_key(|f| (rank(f), key(f)));
    families
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn automatic_prefers_first_installed_candidate() {
        let fonts = installed(&["Menlo", "Hack Nerd Font Mono", "JetBrainsMono Nerd Font", "Monaco"]);
        assert_eq!(automatic(&fonts), Automatic { family: "JetBrainsMono Nerd Font".into(), nerd: true });
        // Installed spelling is kept even when it differs in case/spacing.
        let fonts = installed(&["Menlo", "Berkeley Mono Nerd Font Mono"]);
        assert_eq!(automatic(&fonts).family, "Berkeley Mono Nerd Font Mono");
        let fonts = installed(&["menlo", "JetBrains Mono Nerd Font Mono"]);
        assert_eq!(automatic(&fonts).family, "JetBrains Mono Nerd Font Mono");
    }

    #[test]
    fn automatic_falls_back_to_menlo_without_a_nerd_font() {
        let fonts = installed(&["Menlo", "Monaco", "SF Mono"]);
        assert_eq!(automatic(&fonts), Automatic { family: "Menlo".into(), nerd: false });
        assert_eq!(automatic(&[]), Automatic { family: FALLBACK_FONT.into(), nerd: false });
    }

    #[test]
    fn candidates_are_unique_mono_first_nerd_fonts() {
        let mut keys: Vec<_> = NERD_FONT_CANDIDATES.iter().map(|c| key(c)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), NERD_FONT_CANDIDATES.len());
        assert!(NERD_FONT_CANDIDATES[0].ends_with("Mono"));
        assert!(NERD_FONT_CANDIDATES.iter().all(|c| is_nerd(c)));
        assert!(!is_nerd("Menlo") && !is_nerd("SF Mono") && is_nerd("Iosevka Nerd Font"));
    }

    #[test]
    fn exact_preference_wins_and_missing_one_is_flagged() {
        let fonts = installed(&["Menlo", "Monaco", "Hack Nerd Font Mono"]);
        assert_eq!(resolve("Monaco", &fonts), Resolved { family: "Monaco".into(), missing: false });
        assert_eq!(resolve(" monaco ", &fonts), Resolved { family: "Monaco".into(), missing: false });
        assert_eq!(resolve("", &fonts), Resolved { family: "Hack Nerd Font Mono".into(), missing: false });
        // A removed font falls back to the automatic choice and is flagged.
        assert_eq!(resolve("Removed Mono", &fonts), Resolved { family: "Hack Nerd Font Mono".into(), missing: true });
        assert_eq!(resolve("Removed Mono", &installed(&["Menlo"])), Resolved { family: "Menlo".into(), missing: true });
        let problem = missing_problem("Removed Mono", "Menlo");
        assert!(problem.contains("Removed Mono") && problem.contains("Menlo") && !problem.contains('/'));
    }

    #[test]
    fn monospace_check_and_picker_order() {
        assert!(is_monospace(&[7.8, 7.8, 7.8, 7.8]));
        assert!(!is_monospace(&[3.1, 9.9, 11.0, 7.0]));
        assert!(!is_monospace(&[]));
        assert!(!is_monospace(&[0.0, 0.0]));
        let order = picker_order(installed(&[
            "Menlo",
            ".SF NS Mono",
            "Courier New",
            "Iosevka Nerd Font",
            "Hack Nerd Font Mono",
            "JetBrainsMono Nerd Font Mono",
            "menlo",
        ]));
        assert_eq!(order, installed(&["JetBrainsMono Nerd Font Mono", "Hack Nerd Font Mono", "Iosevka Nerd Font", "Courier New", "Menlo"]));
    }

    #[test]
    fn ui_font_defaults_to_system_and_flags_missing() {
        let fonts = installed(&["Helvetica Neue", "Inter", "Menlo"]);
        assert_eq!(resolve_ui("", &fonts), Resolved { family: UI_SYSTEM_FONT.into(), missing: false });
        assert_eq!(resolve_ui("  ", &fonts), Resolved { family: UI_SYSTEM_FONT.into(), missing: false });
        assert_eq!(resolve_ui(" inter ", &fonts), Resolved { family: "Inter".into(), missing: false });
        // Monospaced families are allowed for the UI too.
        assert_eq!(resolve_ui("Menlo", &fonts), Resolved { family: "Menlo".into(), missing: false });
        // A removed family falls back to the system font, not the terminal's.
        assert_eq!(resolve_ui("Removed Sans", &fonts), Resolved { family: UI_SYSTEM_FONT.into(), missing: true });
    }

    #[test]
    fn ui_picker_lists_text_families_alphabetically() {
        let order = ui_picker_order(installed(&[
            "Menlo",
            ".SF NS",
            "Apple Color Emoji",
            "Inter",
            "Wingdings 2",
            "Helvetica Neue",
            "JetBrainsMono Nerd Font Mono",
            "inter",
            " ",
        ]));
        assert_eq!(order, installed(&["Helvetica Neue", "Inter", "JetBrainsMono Nerd Font Mono", "Menlo"]));
        assert!(!is_ui_candidate(UI_SYSTEM_FONT));
    }
}
