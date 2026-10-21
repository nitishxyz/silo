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
    "laptop",
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
    "info",
    "info-mark",
    "alert",
    "settings-hard-drive",
    "settings-info",
    "settings-keyboard",
    "settings-search",
    "settings-shapes",
    "settings-bell",
    "question",
    "key"
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

/// An icon element: `icon("laptop", 16.0, color)`.
pub fn icon(name: &'static str, size: f32, color: u32) -> gpui::Svg {
    use gpui::{Styled, px, rgb};
    gpui::svg().path(format!("icons/{name}.svg")).size(px(size)).flex_none().text_color(rgb(color))
}
