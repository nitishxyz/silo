//! Embedded SVG icons (`icons/<name>.svg`), monochrome masks tinted by the
//! element's text color.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[
            $((concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "mouse-pointer",
    "frame",
    "upload",
    "laptop",
    "arrow-left-right",
    "waypoints",
    "globe",
    "check",
    "filter",
    "new-session",
    "add-host",
    "close",
    "plus",
    "terminal",
    "chevron",
    "split-right",
    "split-down",
    "zoom",
    "unzoom",
    "eye",
    "back",
    "forward",
    "reload",
    "spinner",
    "lock",
    "lock-off",
    "mic",
    "info",
    "info-mark",
    "alert",
    "settings-hard-drive",
    "settings-info",
    "settings-keyboard",
    "settings-search",
    "settings",
    "settings-shapes",
    "settings-bell",
    "question",
    "key",
    "agent",
    "chevron-down",
    "chevron-up",
    "chevron-right",
    "history",
    "more-vertical",
    "arrow-up",
    "stop",
    "brain",
    "image",
    "list-todo",
    "clock",
    "circle",
    "circle-dot",
    "circle-check",
    "x-circle",
    "file-text",
    "search",
    "folder",
    "pencil",
    "arrow-up-right",
    "copy",
    "rotate-ccw",
    "bot",
    "provider-anthropic",
    "provider-openai",
    "provider-google",
    "provider-xai",
    "provider-openrouter",
    "provider-deepseek",
    "provider-moonshotai",
    "provider-zai",
    "provider-minimax",
    "provider-ollama",
    "provider-lmstudio",
    "provider-higgsfield",
    "provider-fal",
    "provider-elevenlabs",
    "arrow-right",
    "eye-off",
    "plug",
    "sidebar-right",
    "sidebar-left",
    "layers",
    "git-diff",
    "git-branch",
    "message-square",
    "minimize",
    "volume",
    "volume-off",
    "video",
    "clapperboard",
    "earth",
    "sparkles",
    "sticky-note",
    "user",
    "audio-lines",
    "play",
    "pause",
    "rotate-cw",
    "repeat",
    "expand",
    "shrink"
);

#[cfg(test)]
pub fn has_icon(name: &str) -> bool {
    let path = format!("icons/{name}.svg");
    ICONS.iter().any(|(p, _)| *p == path)
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ICONS.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)).collect())
    }
}

/// Allo (`fonts/allo/`, SIL OFL 1.1, by Josh Puckett): the bundled UI
/// font. The static faces (Text optical size, Friendly roundness): nine
/// weights and a true oblique of each, so `FontWeight` and italics select
/// drawn faces. GPUI matches families face by face, which a variable font
/// (one face, axes) would defeat.
macro_rules! fonts {
    ($($name:literal),* $(,)?) => {
        const FONTS: &[&[u8]] = &[$(include_bytes!(concat!("../assets/fonts/allo/Allo-", $name, ".otf"))),*];
    };
}

fonts!(
    "Thin",
    "ThinOblique",
    "ExtraLight",
    "ExtraLightOblique",
    "Light",
    "LightOblique",
    "Regular",
    "Oblique",
    "Medium",
    "MediumOblique",
    "SemiBold",
    "SemiBoldOblique",
    "Bold",
    "BoldOblique",
    "ExtraBold",
    "ExtraBoldOblique",
    "Black",
    "BlackOblique",
);

/// Register the bundled fonts with the text system, before anything
/// resolves a family (they then list among the installed families).
pub fn register_fonts(cx: &gpui::App) {
    if let Err(error) = cx.text_system().add_fonts(FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect()) {
        tracing::warn!("bundled fonts: {error:#}");
    }
}

/// An icon element: `icon("laptop", 16.0, color)`.
pub fn icon(name: &'static str, size: f32, color: u32) -> gpui::Svg {
    use gpui::{Styled, px, rgb};
    gpui::svg().path(format!("icons/{name}.svg")).size(px(size)).flex_none().text_color(rgb(color))
}
