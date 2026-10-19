//! Silo's own terminal preferences (`$SILO_HOME/terminal.json`): theme,
//! font, color overrides, cursor and scrollback. Silo never reads another
//! terminal's configuration; everything here has a built-in default, so the
//! app is complete without any file at all.
//!
//! Colors, cursor and scrollback travel to the server as a
//! [`TerminalConfig`] (the owner's applies to the authoritative terminal);
//! the font is local to this app.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use silo_proto::model::{CursorShape, TerminalConfig};

pub const FILE: &str = "terminal.json";
pub const DEFAULT_THEME: &str = "silo-dark";
/// The default font preference: Automatic (first installed supported Nerd
/// Font, else Menlo), stored as an empty family.
pub const DEFAULT_FONT: &str = "";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// A bundled color theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub id: &'static str,
    pub name: &'static str,
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: [u32; 16],
}

/// Built-in themes, compiled into the app (no theme files are read).
pub const THEMES: &[Palette] = &[
    Palette {
        id: "silo-dark",
        name: "Silo Dark",
        background: 0x0016_1719,
        foreground: 0x00d6_d8dc,
        cursor: 0x00e8_e9ec,
        selection: 0x0074_bdf8,
        ansi: [
            0x001d_1f21,
            0x00e0_6c75,
            0x0098_c379,
            0x00e5_c07b,
            0x0061_afef,
            0x00c6_78dd,
            0x0056_b6c2,
            0x00ab_b2bf,
            0x005c_6370,
            0x00ef_7b84,
            0x00a9_d48a,
            0x00f0_cd8c,
            0x0074_bdf8,
            0x00d5_8ae8,
            0x0066_c7d3,
            0x00e6_e9ef,
        ],
    },
    Palette {
        id: "silo-light",
        name: "Silo Light",
        background: 0x00fa_fafa,
        foreground: 0x0038_3a42,
        cursor: 0x0052_6fff,
        selection: 0x0052_6fff,
        ansi: [
            0x0038_3a42,
            0x00e4_5649,
            0x0050_a14f,
            0x00c1_8401,
            0x0001_84bc,
            0x00a6_26a4,
            0x0009_97b3,
            0x00a0_a1a7,
            0x004f_525e,
            0x00e0_6c75,
            0x0040_9140,
            0x00b0_7a00,
            0x0052_6fff,
            0x00b0_5fc0,
            0x0000_8b9c,
            0x00ff_ffff,
        ],
    },
    Palette {
        id: "ayu-dark",
        name: "Ayu Dark",
        background: 0x000b_0e14,
        foreground: 0x00bf_bdb6,
        cursor: 0x00e6_b450,
        selection: 0x0040_9fff,
        ansi: [
            0x0011_151c,
            0x00ea_6c73,
            0x007f_d962,
            0x00f9_af4f,
            0x0053_bdfa,
            0x00cd_a1fa,
            0x0090_e1c6,
            0x00c7_c7c7,
            0x0068_6868,
            0x00f0_7178,
            0x00aa_d94c,
            0x00ff_b454,
            0x0059_c2ff,
            0x00d2_a6ff,
            0x0095_e6cb,
            0x00ff_ffff,
        ],
    },
    Palette {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        background: 0x001e_1e2e,
        foreground: 0x00cd_d6f4,
        cursor: 0x00f5_e0dc,
        selection: 0x0089_b4fa,
        ansi: [
            0x0045_475a,
            0x00f3_8ba8,
            0x00a6_e3a1,
            0x00f9_e2af,
            0x0089_b4fa,
            0x00f5_c2e7,
            0x0094_e2d5,
            0x00ba_c2de,
            0x0058_5b70,
            0x00f3_8ba8,
            0x00a6_e3a1,
            0x00f9_e2af,
            0x0089_b4fa,
            0x00f5_c2e7,
            0x0094_e2d5,
            0x00a6_adc8,
        ],
    },
    Palette {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        background: 0x00ef_f1f5,
        foreground: 0x004c_4f69,
        cursor: 0x00dc_8a78,
        selection: 0x001e_66f5,
        ansi: [
            0x005c_5f77,
            0x00d2_0f39,
            0x0040_a02b,
            0x00df_8e1d,
            0x001e_66f5,
            0x00ea_76cb,
            0x0017_9299,
            0x00ac_b0be,
            0x006c_6f85,
            0x00d2_0f39,
            0x0040_a02b,
            0x00df_8e1d,
            0x001e_66f5,
            0x00ea_76cb,
            0x0017_9299,
            0x00bc_c0cc,
        ],
    },
    Palette {
        id: "nord",
        name: "Nord",
        background: 0x002e_3440,
        foreground: 0x00d8_dee9,
        cursor: 0x00d8_dee9,
        selection: 0x0088_c0d0,
        ansi: [
            0x003b_4252,
            0x00bf_616a,
            0x00a3_be8c,
            0x00eb_cb8b,
            0x0081_a1c1,
            0x00b4_8ead,
            0x0088_c0d0,
            0x00e5_e9f0,
            0x004c_566a,
            0x00bf_616a,
            0x00a3_be8c,
            0x00eb_cb8b,
            0x0081_a1c1,
            0x00b4_8ead,
            0x008f_bcbb,
            0x00ec_eff4,
        ],
    },
];

#[must_use]
pub fn palette(id: &str) -> Option<&'static Palette> {
    THEMES.iter().find(|p| p.id == id)
}

/// Optional per-color overrides on top of the theme, as `#rrggbb`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSlot {
    Foreground,
    Background,
    Cursor,
    Selection,
}

impl ColorSlot {
    pub const ALL: [Self; 4] = [Self::Foreground, Self::Background, Self::Cursor, Self::Selection];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Foreground => "Foreground",
            Self::Background => "Background",
            Self::Cursor => "Cursor",
            Self::Selection => "Selection",
        }
    }
}

impl ColorOverrides {
    #[must_use]
    pub fn get(&self, slot: ColorSlot) -> Option<&String> {
        match slot {
            ColorSlot::Foreground => self.foreground.as_ref(),
            ColorSlot::Background => self.background.as_ref(),
            ColorSlot::Cursor => self.cursor.as_ref(),
            ColorSlot::Selection => self.selection.as_ref(),
        }
    }

    pub fn set(&mut self, slot: ColorSlot, value: Option<u32>) {
        let value = value.map(hex_string);
        match slot {
            ColorSlot::Foreground => self.foreground = value,
            ColorSlot::Background => self.background = value,
            ColorSlot::Cursor => self.cursor = value,
            ColorSlot::Selection => self.selection = value,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalPrefs {
    pub theme: String,
    /// Exact installed family name, or empty for Automatic.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub font_family: String,
    pub font_size: f32,
    pub colors: ColorOverrides,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub scrollback_lines: u64,
}

impl Default for TerminalPrefs {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.into(),
            font_family: DEFAULT_FONT.into(),
            font_size: DEFAULT_FONT_SIZE,
            colors: ColorOverrides::default(),
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            scrollback_lines: TerminalConfig::DEFAULT_SCROLLBACK,
        }
    }
}

/// Everything derived from the preferences that the app renders with.
#[derive(Clone, Debug, PartialEq)]
pub struct Effective {
    pub config: TerminalConfig,
    pub selection: u32,
    pub font_family: String,
    pub font_size: f32,
    pub theme_name: &'static str,
}

impl TerminalPrefs {
    /// Repair out-of-range or unknown values; returns what was changed.
    #[must_use]
    pub fn validated(mut self) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        if palette(&self.theme).is_none() {
            problems.push(format!("theme \"{}\" is not built in; using Silo Dark", self.theme));
            self.theme = DEFAULT_THEME.into();
        }
        let family = self.font_family.trim();
        if family.len() != self.font_family.len() {
            self.font_family = family.to_owned();
        }
        if !self.font_size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&self.font_size) {
            let size = if self.font_size.is_finite() { self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE) } else { DEFAULT_FONT_SIZE };
            problems.push(format!("font size {} out of range; using {size}", self.font_size));
            self.font_size = size;
        }
        for slot in ColorSlot::ALL {
            if let Some(value) = self.colors.get(slot).cloned() {
                match parse_hex(&value) {
                    Some(color) => self.colors.set(slot, Some(color)),
                    None => {
                        problems.push(format!("{} color \"{value}\" is not #rrggbb; using the theme's", slot.label().to_lowercase()));
                        self.colors.set(slot, None);
                    }
                }
            }
        }
        let lines = self.scrollback_lines.clamp(TerminalConfig::MIN_SCROLLBACK, TerminalConfig::MAX_SCROLLBACK);
        if lines != self.scrollback_lines {
            problems.push(format!("scrollback {} out of range; using {lines}", self.scrollback_lines));
            self.scrollback_lines = lines;
        }
        (self, problems)
    }

    #[must_use]
    pub fn effective(&self) -> Effective {
        let theme = palette(&self.theme).unwrap_or(&THEMES[0]);
        let pick = |slot, fallback| self.colors.get(slot).and_then(|v| parse_hex(v)).unwrap_or(fallback);
        Effective {
            config: TerminalConfig {
                foreground: pick(ColorSlot::Foreground, theme.foreground),
                background: pick(ColorSlot::Background, theme.background),
                cursor: pick(ColorSlot::Cursor, theme.cursor),
                palette: theme.ansi,
                cursor_shape: self.cursor_shape,
                cursor_blink: self.cursor_blink,
                scrollback_lines: self.scrollback_lines,
            }
            .sanitized(),
            selection: pick(ColorSlot::Selection, theme.selection),
            font_family: self.font_family.clone(),
            font_size: self.font_size,
            theme_name: theme.name,
        }
    }
}

#[must_use]
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

/// Loaded preferences plus a user-facing problem description, if any.
pub struct Loaded {
    pub prefs: TerminalPrefs,
    pub problem: Option<String>,
}

/// Read `terminal.json`. A missing file is the normal first-run case
/// (defaults, no problem); an unreadable or invalid one falls back to
/// defaults and reports why, leaving the file untouched until the next save.
#[must_use]
pub fn load(home: &Path) -> Loaded {
    let path = path(home);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded { prefs: TerminalPrefs::default(), problem: None };
        }
        Err(e) => {
            return Loaded { prefs: TerminalPrefs::default(), problem: Some(format!("Could not read terminal settings: {e}")) };
        }
    };
    match serde_json::from_slice::<TerminalPrefs>(&bytes) {
        Ok(prefs) => {
            let (prefs, problems) = prefs.validated();
            let problem = (!problems.is_empty()).then(|| format!("Terminal settings: {}", problems.join("; ")));
            Loaded { prefs, problem }
        }
        Err(e) => Loaded {
            prefs: TerminalPrefs::default(),
            problem: Some(format!("Terminal settings file is invalid ({e}); using defaults until you change a setting")),
        },
    }
}

/// Atomically replace `terminal.json` (write a temp file, fsync, rename).
///
/// # Errors
/// Any filesystem failure; the previous file is left intact.
pub fn save(home: &Path, prefs: &TerminalPrefs) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let target = path(home);
    let tmp = home.join(format!(".{FILE}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut bytes = serde_json::to_vec_pretty(prefs).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[must_use]
pub fn parse_hex(value: &str) -> Option<u32> {
    let v = value.trim().trim_start_matches('#');
    let v = if v.len() == 3 { v.chars().flat_map(|c| [c, c]).collect::<String>() } else { v.to_owned() };
    if v.len() != 6 || !v.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(&v, 16).ok()
}

#[must_use]
pub fn hex_string(color: u32) -> String {
    format!("#{:06x}", color & 0x00ff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("silo-prefs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn defaults_are_standalone_silo_dark() {
        let prefs = TerminalPrefs::default();
        let effective = prefs.effective();
        // The built-in theme is the one the server uses before any client
        // reports a configuration.
        assert_eq!(effective.config, TerminalConfig::default());
        assert_eq!(effective.font_family, DEFAULT_FONT, "Automatic");
        assert_eq!(effective.theme_name, "Silo Dark");
        assert_eq!(prefs.clone().validated(), (prefs, Vec::new()));
        for theme in THEMES {
            assert!(palette(theme.id).is_some());
        }
    }

    #[test]
    fn built_in_themes_are_a_small_coherent_set() {
        assert!((4..=6).contains(&THEMES.len()));
        let mut ids: Vec<_> = THEMES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEMES.len(), "unique ids");
        let light = |t: &Palette| TerminalConfig { background: t.background, ..TerminalConfig::default() }.is_light();
        assert!(THEMES.iter().any(light) && THEMES.iter().any(|t| !light(t)), "dark and light themes");
        for t in THEMES {
            assert_ne!(t.background, t.foreground, "{}", t.name);
            // Every theme applies end to end through the same preferences path.
            let prefs = TerminalPrefs { theme: t.id.into(), ..TerminalPrefs::default() };
            assert_eq!(prefs.clone().validated().1, Vec::<String>::new());
            assert_eq!(prefs.effective().config.background, t.background);
            assert_eq!(prefs.effective().theme_name, t.name);
        }
    }

    #[test]
    fn missing_file_is_defaults_without_problem() {
        let home = temp_home("missing");
        let loaded = load(&home);
        assert_eq!(loaded.prefs, TerminalPrefs::default());
        assert!(loaded.problem.is_none());
        assert!(!home.exists(), "loading must not create anything");
    }

    #[test]
    fn round_trip_and_atomic_save() {
        let home = temp_home("roundtrip");
        let mut prefs = TerminalPrefs {
            theme: "nord".into(),
            font_size: 15.0,
            cursor_shape: CursorShape::Bar,
            cursor_blink: false,
            scrollback_lines: 50_000,
            ..TerminalPrefs::default()
        };
        prefs.colors.set(ColorSlot::Background, Some(0x0010_2030));
        save(&home, &prefs).unwrap();
        let loaded = load(&home);
        assert_eq!(loaded.prefs, prefs);
        assert!(loaded.problem.is_none());
        let text = std::fs::read_to_string(path(&home)).unwrap();
        assert!(text.contains("\"background\": \"#102030\""), "{text}");
        assert!(text.contains("\"cursor_shape\": \"bar\""), "{text}");
        let effective = loaded.prefs.effective();
        assert_eq!(effective.config.background, 0x0010_2030);
        assert_eq!(effective.config.foreground, palette("nord").unwrap().foreground);
        // Only the final file remains; no temp files left behind.
        let names: Vec<_> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE)]);
        // A failed save (target is a directory) keeps the previous file.
        let blocked = temp_home("blocked");
        std::fs::create_dir_all(path(&blocked)).unwrap();
        assert!(save(&blocked, &prefs).is_err());
        assert!(std::fs::read_dir(&blocked).unwrap().count() == 1);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(blocked);
    }

    #[test]
    fn invalid_values_are_repaired_and_reported() {
        let home = temp_home("invalid");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            path(&home),
            r##"{"theme":"nope","font_size":200,"font_family":"  ","colors":{"cursor":"blue","foreground":"#ABC"},"scrollback_lines":5,"extra":1}"##,
        )
        .unwrap();
        let loaded = load(&home);
        let p = &loaded.prefs;
        assert_eq!(p.theme, DEFAULT_THEME);
        assert!((p.font_size - MAX_FONT_SIZE).abs() < f32::EPSILON);
        assert_eq!(p.font_family, DEFAULT_FONT);
        assert_eq!(p.colors.cursor, None);
        assert_eq!(p.colors.foreground.as_deref(), Some("#aabbcc"));
        assert_eq!(p.scrollback_lines, TerminalConfig::MIN_SCROLLBACK);
        let problem = loaded.problem.unwrap();
        assert!(problem.contains("not built in") && problem.contains("cursor color"), "{problem}");
        assert!(!problem.contains(&home.display().to_string()), "no paths in messages: {problem}");

        std::fs::write(path(&home), b"{ not json").unwrap();
        let loaded = load(&home);
        assert_eq!(loaded.prefs, TerminalPrefs::default());
        assert!(loaded.problem.unwrap().contains("invalid"));
        // The broken file is not overwritten by loading.
        assert_eq!(std::fs::read(path(&home)).unwrap(), b"{ not json");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn font_family_persists_exactly_and_automatic_is_implicit() {
        let home = temp_home("font");
        save(&home, &TerminalPrefs::default()).unwrap();
        let text = std::fs::read_to_string(path(&home)).unwrap();
        assert!(!text.contains("font_family"), "Automatic is not written: {text}");
        assert_eq!(load(&home).prefs.font_family, DEFAULT_FONT);

        let prefs = TerminalPrefs { font_family: "Berkeley Mono Nerd Font Mono".into(), ..TerminalPrefs::default() };
        save(&home, &prefs).unwrap();
        let loaded = load(&home);
        assert_eq!(loaded.prefs.font_family, "Berkeley Mono Nerd Font Mono");
        assert!(loaded.problem.is_none());
        // A family that is not installed is kept as written; the renderer
        // flags it and falls back (see `fonts::resolve`).
        std::fs::write(path(&home), r#"{"font_family":"Removed Mono"}"#).unwrap();
        assert_eq!(load(&home).prefs.font_family, "Removed Mono");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn no_external_terminal_config_is_consulted() {
        // A Ghostty config in a fake HOME/XDG must not influence anything.
        let fake = temp_home("fake-home");
        let dir = fake.join(concat!(".config/", "ghostty"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "background = #ff0000\nfont-family = Berkeley Mono\n").unwrap();
        let home = temp_home("silo-home");
        let loaded = load(&home);
        assert_eq!(loaded.prefs.effective().config.background, TerminalConfig::default().background);
        assert_eq!(loaded.prefs.effective().font_family, DEFAULT_FONT);
        let source = include_str!("terminal_prefs.rs").to_owned() + include_str!("theme.rs");
        for needle in [concat!("com.mitchellh", ".ghostty"), concat!(".config/", "ghostty"), concat!("Ghostty", ".app")] {
            assert!(!source.contains(needle), "external config path {needle} referenced");
        }
        let _ = std::fs::remove_dir_all(fake);
    }
}
