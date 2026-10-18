//! The app theme: terminal colors and font from the user's Ghostty config
//! (when present) or the built-in dark theme, with chrome (titlebar, tabs,
//! pill, popovers) derived from the terminal background and foreground so
//! the window reads as one surface.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use silo_vt::{ColorScheme, TerminalColors};

/// Chrome text uses the system UI font.
pub const UI_FONT: &str = ".SystemUIFont";

const DEFAULT_ANSI: [u32; 16] = [
    0x001d_1f21, 0x00e0_6c75, 0x0098_c379, 0x00e5_c07b, 0x0061_afef, 0x00c6_78dd, 0x0056_b6c2, 0x00ab_b2bf,
    0x005c_6370, 0x00ef_7b84, 0x00a9_d48a, 0x00f0_cd8c, 0x0074_bdf8, 0x00d5_8ae8, 0x0066_c7d3, 0x00e6_e9ef,
];

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub selection: Option<u32>,
    pub ansi: [u32; 16],
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub opacity: f32,
    /// Where the colors came from, for logs.
    pub source: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: 0x0016_1719,
            foreground: 0x00d6_d8dc,
            cursor: 0x00e8_e9ec,
            selection: None,
            ansi: DEFAULT_ANSI,
            font_family: None,
            font_size: None,
            opacity: 1.0,
            source: "built-in".into(),
        }
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Install the theme once at startup (before any window renders).
pub fn install(theme: Theme) {
    let _ = THEME.set(theme);
}

fn current() -> &'static Theme {
    THEME.get_or_init(Theme::default)
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
    luminance(current().background) > 0.5
}

// ---- palette accessors (chrome derives from bg/fg) ----

#[must_use]
pub fn background() -> u32 {
    current().background
}
#[must_use]
pub fn foreground() -> u32 {
    current().foreground
}
/// Titlebar: the same surface as the terminal.
#[must_use]
pub fn chrome() -> u32 {
    current().background
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
#[must_use]
pub fn muted() -> u32 {
    mix(background(), foreground(), 0.55)
}
/// The theme's bright blue.
#[must_use]
pub fn accent() -> u32 {
    current().ansi[12]
}
#[must_use]
pub fn danger() -> u32 {
    current().ansi[1]
}
#[must_use]
pub fn selection() -> u32 {
    current().selection.unwrap_or_else(accent)
}
/// Text on an accent-colored button.
#[must_use]
pub fn on_accent() -> u32 {
    if luminance(accent()) > 0.6 { 0x0000_0000 } else { 0x00ff_ffff }
}

#[must_use]
pub fn terminal_colors() -> TerminalColors {
    let t = current();
    TerminalColors {
        scheme: if is_light() { ColorScheme::Light } else { ColorScheme::Dark },
        foreground: t.foreground,
        background: t.background,
        cursor: t.cursor,
        ansi: t.ansi,
    }
}

#[must_use]
pub fn font() -> (Option<String>, Option<f32>) {
    let t = current();
    (t.font_family.clone(), t.font_size)
}

// ---- Ghostty config ----

/// Ghostty config files in load order (later wins).
fn config_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    vec![xdg.join("ghostty/config"), home.join("Library/Application Support/com.mitchellh.ghostty/config")]
}

fn theme_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    let mut dirs = vec![xdg.join("ghostty/themes")];
    for app in ["/Applications/Ghostty.app", &format!("{}/Applications/Ghostty.app", home.display())] {
        dirs.push(Path::new(app).join("Contents/Resources/ghostty/themes"));
    }
    if let Some(dir) = std::env::var_os("GHOSTTY_RESOURCES_DIR") {
        dirs.push(PathBuf::from(dir).join("themes"));
    }
    dirs
}

/// `(key, value)` pairs of a Ghostty config file, comments and quotes
/// stripped.
fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

fn hex(value: &str) -> Option<u32> {
    let v = value.trim().trim_start_matches('#');
    let v = if v.len() == 3 { v.chars().flat_map(|c| [c, c]).collect::<String>() } else { v.to_owned() };
    (v.len() == 6).then(|| u32::from_str_radix(&v, 16).ok()).flatten()
}

fn system_is_dark() -> bool {
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().eq_ignore_ascii_case("dark"))
}

/// Pick the side of a `light:X,dark:Y` pair for the system appearance.
fn appearance_variant(value: &str, dark: bool) -> String {
    if !value.contains("light:") && !value.contains("dark:") {
        return value.to_owned();
    }
    let mut light_v = None;
    let mut dark_v = None;
    for part in value.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("light:") {
            light_v = Some(v.trim().to_owned());
        } else if let Some(v) = part.strip_prefix("dark:") {
            dark_v = Some(v.trim().to_owned());
        }
    }
    let pick = if dark { dark_v.clone().or(light_v.clone()) } else { light_v.clone().or(dark_v.clone()) };
    pick.unwrap_or_else(|| value.to_owned())
}

fn find_theme(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_owned());
    }
    for dir in dirs {
        let exact = dir.join(name);
        if exact.is_file() {
            return Some(exact);
        }
        // Ghostty matches theme names case-insensitively on macOS.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().eq_ignore_ascii_case(name) {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

fn apply(theme: &mut Theme, pairs: &[(String, String)]) {
    for (key, value) in pairs {
        match key.as_str() {
            "background" => theme.background = hex(value).unwrap_or(theme.background),
            "foreground" => theme.foreground = hex(value).unwrap_or(theme.foreground),
            "cursor-color" => theme.cursor = hex(value).unwrap_or(theme.cursor),
            "selection-background" => theme.selection = hex(value).or(theme.selection),
            "palette" => {
                if let Some((index, color)) = value.split_once('=')
                    && let (Ok(index), Some(color)) = (index.trim().parse::<usize>(), hex(color))
                    && index < 16
                {
                    theme.ansi[index] = color;
                }
            }
            "font-family" if !value.is_empty() => {
                // Ghostty allows repeating font-family for fallbacks; the
                // first one is the primary face.
                if theme.font_family.is_none() {
                    theme.font_family = Some(value.clone());
                }
            }
            "font-size" => theme.font_size = value.parse().ok().filter(|s: &f32| *s > 4.0 && *s < 96.0).or(theme.font_size),
            "background-opacity" => theme.opacity = value.parse().ok().map_or(theme.opacity, |o: f32| o.clamp(0.0, 1.0)),
            _ => {}
        }
    }
}

/// Load the theme from Ghostty's config (theme file first, then the
/// config's own overrides), else the built-in theme.
#[must_use]
pub fn load_ghostty() -> Theme {
    let mut pairs = Vec::new();
    let mut sources = Vec::new();
    for path in config_paths() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            pairs.extend(parse(&text));
            sources.push(path.display().to_string());
        }
    }
    let mut theme = Theme::default();
    if pairs.is_empty() {
        return theme;
    }
    let dark = system_is_dark();
    if let Some((_, name)) = pairs.iter().rev().find(|(k, _)| k == "theme") {
        let name = appearance_variant(name, dark);
        match find_theme(&name, &theme_dirs()) {
            Some(path) => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    apply(&mut theme, &parse(&text));
                    sources.push(format!("theme {}", path.display()));
                }
            }
            None => tracing::warn!("ghostty theme {name:?} not found"),
        }
    }
    apply(&mut theme, &pairs);
    theme.source = sources.join(", ");
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ghostty_config_and_theme() {
        let dir = std::env::temp_dir().join(format!("silo-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Ayu"), "palette = 12=#59c2ff\nbackground = #0b0e14\nforeground = #bfbdb6\n").unwrap();
        let found = find_theme("ayu", std::slice::from_ref(&dir)).unwrap();
        let mut theme = Theme::default();
        apply(&mut theme, &parse(&std::fs::read_to_string(found).unwrap()));
        apply(&mut theme, &parse("# c\nfont-family = \"Berkeley Mono\"\nfont-size = 17\nforeground = #ffffff\npalette = 1 = #ff0000\n"));
        assert_eq!(theme.background, 0x000b_0e14);
        assert_eq!(theme.foreground, 0x00ff_ffff);
        assert_eq!(theme.ansi[12], 0x0059_c2ff);
        assert_eq!(theme.ansi[1], 0x00ff_0000);
        assert_eq!(theme.font_family.as_deref(), Some("Berkeley Mono"));
        assert_eq!(theme.font_size, Some(17.0));
        assert_eq!(appearance_variant("light:Ayu Light,dark:Ayu", true), "Ayu");
        assert_eq!(appearance_variant("light:Ayu Light,dark:Ayu", false), "Ayu Light");
        let _ = std::fs::remove_dir_all(dir);
    }
}
