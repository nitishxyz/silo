//! Floating surfaces (menus, popovers, tooltips, pickers, the palette,
//! dialogs and sheets) are frosted glass, as on macOS: a translucent tint
//! over an in-window backdrop blur (the GPUI fork's `backdrop_blur`) of
//! whatever is beneath, the app's own content and lower surfaces included,
//! with a hairline border, a soft shadow and continuous corners.
//!
//! The tint's alpha is the least at which the theme's text reads at 4.5:1
//! whatever is behind (black, white or the theme background, plain or under
//! a selected row) and a muted step of it does too (3:1 on a selected row). In a Frosted window the
//! transparent parts behind are already veiled, so opaque black and white
//! stay the extremes. Without backdrop support or with Reduce transparency
//! on, surfaces are opaque and unblurred.
//!
//! Colours that differ on glass (secondary text, row and control fills)
//! follow [`enter`]: build a surface's content inside its scope.

use std::cell::Cell;

use gpui::{BoxShadow, Styled, hsla, point, px, rgb};

use crate::{
    material,
    theme::{self, Continuous},
};

/// Gaussian radius of the blur, in points (a macOS menu's).
pub const BLUR: f32 = 20.0;
/// Saturation of the blurred backdrop, so colour behind shows through the
/// tint as vivid as in a native menu.
pub const SATURATION: f32 = 1.8;
/// The least tint alpha: below it the surface reads as a hole.
pub const FLOOR: f32 = 0.72;
/// Secondary text must read at most this far from the tint toward the ink,
/// so it stays a distinct step from primary text.
pub const MUTED_REACH: f32 = 0.85;
/// Ink tint of a selected row on glass ([`theme::row_fill`]), the densest
/// fill text sits on.
pub const SELECTED: f32 = 0.14;

/// One frame's glass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glass {
    /// Tint colour, painted at `alpha`.
    pub tint: u32,
    /// 1 when opaque.
    pub alpha: f32,
    /// Secondary text, reading on every backdrop.
    pub muted: u32,
}

impl Glass {
    /// Translucent, over a backdrop blur.
    #[must_use]
    pub fn frosted(&self) -> bool {
        self.alpha < 1.0
    }

    /// What the glass looks like over the theme background.
    #[must_use]
    pub fn look(&self, theme_bg: u32) -> u32 {
        material::over(self.tint, self.alpha, theme_bg)
    }
}

/// Whether the renderer blurs in-window backdrops: GPUI's Metal renderer
/// does; the others skip the primitive.
#[must_use]
pub fn backdrop_supported() -> bool {
    cfg!(target_os = "macos")
}

/// The extremes glass of `tint` at `alpha` can show: over the theme
/// background, black and white; then each under a selected row.
fn backdrops(tint: u32, alpha: f32, theme_bg: u32, fg: u32) -> [u32; 6] {
    let plain = [theme_bg, 0, 0x00ff_ffff].map(|b| material::over(tint, alpha, b));
    let selected = plain.map(|b| theme::mix(b, fg, SELECTED));
    [plain[0], plain[1], plain[2], selected[0], selected[1], selected[2]]
}

/// `text` reads at 4.5:1 on every one of `bgs`, all on the same side of it
/// (so it also reads on everything between them).
#[must_use]
pub fn reads_over(text: u32, bgs: &[u32]) -> bool {
    let l = theme::relative_luminance(text);
    let side = |b: &u32| theme::relative_luminance(*b) > l;
    bgs.iter().all(|&b| theme::contrast(text, b) >= theme::TEXT_CONTRAST) && (bgs.iter().all(side) || !bgs.iter().any(side))
}

/// Glass of `tint` for text `fg` over a window of theme background
/// `theme_bg`. `available`: the backdrop can be blurred; else, and where
/// no alpha reads, opaque. Secondary text reads on selected rows too.
#[must_use]
pub fn glass(tint: u32, theme_bg: u32, fg: u32, available: bool) -> Glass {
    let opaque = Glass { tint, alpha: 1.0, muted: theme::toward_ink(tint, fg, 0.55, theme::TEXT_CONTRAST) };
    if !available {
        return opaque;
    }
    let reach = theme::mix(tint, fg, MUTED_REACH);
    (0..)
        .map(|step| FLOOR + 0.01 * step as f32)
        .take_while(|&a| a < 1.0)
        .find_map(|alpha| {
            let bgs = backdrops(tint, alpha, theme_bg, fg);
            (reads_over(fg, &bgs) && reads_over(reach, &bgs[..3])).then(|| Glass { tint, alpha, muted: muted_over(tint, fg, &bgs) })
        })
        .unwrap_or(opaque)
}

/// Secondary text: 4.5:1 on the plain backdrops, and at least 3:1 under a
/// selected row (4.5 there too would leave it no step from primary text).
fn muted_over(tint: u32, fg: u32, bgs: &[u32; 6]) -> u32 {
    let plain = theme::reading(tint, fg, &bgs[..3], 0.55, theme::TEXT_CONTRAST);
    theme::reading(plain, fg, &bgs[3..], 0.0, theme::ICON_CONTRAST)
}

type Key = (u32, u32, u32, bool);

thread_local! {
    static AVAILABLE: Cell<bool> = const { Cell::new(false) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static CACHE: Cell<Option<(Key, Glass)>> = const { Cell::new(None) };
}

/// Set by the workspace each frame: the backdrop can be blurred
/// ([`backdrop_supported`] and Reduce transparency off).
pub fn set_available(available: bool) {
    AVAILABLE.set(available);
}

/// This frame's glass: the theme's popover colour (the window veil's look
/// when frosted) per [`glass`].
#[must_use]
pub fn current() -> Glass {
    let key = (theme::veils().map_or_else(theme::popover, |v| v.overlay), theme::background(), theme::foreground(), AVAILABLE.get());
    if let Some((k, g)) = CACHE.get()
        && k == key
    {
        return g;
    }
    let g = glass(key.0, key.1, key.2, key.3);
    CACHE.set(Some((key, g)));
    g
}

/// While the guard lives, colours are for content on glass
/// ([`theme::muted`], the row and control fills).
#[must_use]
pub fn enter() -> Scope {
    DEPTH.set(DEPTH.get() + 1);
    Scope(())
}

pub struct Scope(());

impl Drop for Scope {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get().saturating_sub(1));
    }
}

/// Inside a [`Scope`].
#[must_use]
pub fn inside() -> bool {
    DEPTH.get() > 0
}

/// Ink tints for fills on glass, rather than solid steps.
#[must_use]
pub fn tinted() -> bool {
    inside() && current().frosted()
}

fn shadows(sheet: bool) -> Vec<BoxShadow> {
    let light = theme::is_light();
    let shadow = |y: f32, blur: f32, dark: f32, lit: f32| BoxShadow {
        color: hsla(0.0, 0.0, 0.0, if light { lit } else { dark }),
        offset: point(px(0.0), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.0),
        inset: false,
    };
    if sheet {
        vec![shadow(18.0, 48.0, 0.45, 0.2), shadow(2.0, 6.0, 0.24, 0.08)]
    } else {
        vec![shadow(8.0, 24.0, 0.36, 0.14), shadow(1.0, 3.0, 0.24, 0.08)]
    }
}

/// Borders and separators on glass: an ink tint, else the theme border.
#[must_use]
pub fn hairline() -> gpui::Rgba {
    if current().frosted() { rgb(theme::foreground()).opacity(if theme::is_light() { 0.12 } else { 0.16 }) } else { rgb(theme::border()) }
}

fn surface<E: Styled>(el: E, radius: f32, sheet: bool) -> E {
    let g = current();
    let el = el.continuous(radius).bg(rgb(g.tint).opacity(g.alpha)).border_1().border_color(hairline()).shadow(shadows(sheet));
    if g.frosted() { el.backdrop_blur(px(BLUR)).backdrop_saturation(SATURATION) } else { el }
}

/// The glass style.
pub trait Glassy: Styled + Sized {
    /// Menus, popovers, pickers, tooltips and cards floating over content.
    #[must_use]
    fn glass(self, radius: f32) -> Self {
        surface(self, radius, false)
    }

    /// Dialogs, sheets and the palette: a deeper shadow.
    #[must_use]
    fn glass_sheet(self, radius: f32) -> Self {
        surface(self, radius, true)
    }
}

impl<T: Styled> Glassy for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// (background, foreground) of the built-in themes.
    const THEMES: [(u32, u32); 11] = [
        (0x0016_1719, 0x00d6_d8dc),
        (0x00fa_fafa, 0x0038_3a42),
        (0x0000_0000, 0x00d4_d4d8),
        (0x000b_0e14, 0x00bf_bdb6),
        (0x001e_1e2e, 0x00cd_d6f4),
        (0x00ef_f1f5, 0x004c_4f69),
        (0x002e_3440, 0x00d8_dee9),
        (0x001a_1b26, 0x00c0_caf5),
        (0x0028_2828, 0x00eb_dbb2),
        (0x0028_2a36, 0x00f8_f8f2),
        (0x00fa_f4ed, 0x0057_5279),
    ];

    fn popover(bg: u32, fg: u32) -> u32 {
        theme::mix(bg, fg, if theme::light(bg) { 0.03 } else { 0.05 })
    }

    /// The least contrast of `text` on the glass over many backdrops,
    /// plain or (`selected`) under a selected row.
    fn worst(g: &Glass, fg: u32, text: u32, selected: bool) -> f32 {
        let mut backdrops: Vec<u32> = (0..=255u32).step_by(5).map(|v| v * 0x0001_0101).collect();
        backdrops.extend([0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ff00, 0x00ff_00ff, 0x0000_ffff, 0x003a_6ea5]);
        backdrops
            .iter()
            .map(|&b| material::over(g.tint, g.alpha, b))
            .map(|c| if selected { theme::mix(c, fg, SELECTED) } else { c })
            .map(|c| theme::contrast(text, c))
            .fold(f32::MAX, f32::min)
    }

    #[test]
    fn text_reads_on_glass_whatever_is_behind() {
        let mut frosted = 0;
        for (bg, fg) in THEMES {
            let g = glass(popover(bg, fg), bg, fg, true);
            assert!(g.alpha >= FLOOR && g.alpha <= 1.0, "{bg:06x}: {}", g.alpha);
            if g.frosted() {
                frosted += 1;
                for selected in [false, true] {
                    assert!(worst(&g, fg, fg, selected) >= theme::TEXT_CONTRAST, "{bg:06x} fg at {}", g.alpha);
                }
                assert!(worst(&g, fg, g.muted, false) >= theme::TEXT_CONTRAST, "{bg:06x} muted at {}", g.alpha);
                assert!(worst(&g, fg, g.muted, true) >= theme::ICON_CONTRAST, "{bg:06x} selected muted at {}", g.alpha);
            }
        }
        // Only the themes whose text is low in contrast go opaque.
        assert!(frosted >= THEMES.len() - 2, "{frosted}");
    }

    #[test]
    fn glass_is_as_clear_as_legibility_allows() {
        for (bg, fg) in THEMES {
            let g = glass(popover(bg, fg), bg, fg, true);
            let reach = theme::mix(g.tint, fg, MUTED_REACH);
            if g.frosted() && g.alpha > FLOOR {
                let bgs = backdrops(g.tint, g.alpha - 0.01, bg, fg);
                assert!(!(reads_over(fg, &bgs) && reads_over(reach, &bgs[..3])), "{bg:06x}: {} is not the least", g.alpha);
            }
        }
        // The default dark and light themes: a dense smoke over a white
        // page, not a slab, and secondary text a visible step from primary.
        for (bg, fg) in [THEMES[0], THEMES[1]] {
            let g = glass(popover(bg, fg), bg, fg, true);
            assert!(g.alpha < 0.86, "{bg:06x}: {}", g.alpha);
            assert!(theme::contrast(g.muted, fg) > 1.3, "{bg:06x}: muted {:06x}", g.muted);
        }
    }

    #[test]
    fn opaque_without_backdrop_support_or_with_reduced_transparency() {
        let (bg, fg) = THEMES[0];
        let g = glass(popover(bg, fg), bg, fg, false);
        assert!(!g.frosted());
        assert_eq!(g.tint, popover(bg, fg));
        assert_eq!(g.look(bg), g.tint);
        assert!(theme::contrast(g.muted, g.tint) >= theme::TEXT_CONTRAST);
        // A text colour no tint alpha makes legible falls back to opaque.
        let g = glass(0x0080_8080, 0x0080_8080, 0x0090_9090, true);
        assert!(!g.frosted());
    }

    /// The alpha and muted ink per built-in theme, for review:
    /// `cargo nextest run -p silo-app -E 'test(glass_table)' --no-capture`.
    #[test]
    fn glass_table() {
        for (bg, fg) in THEMES {
            let g = glass(popover(bg, fg), bg, fg, true);
            println!("{bg:06x}: tint {:06x} alpha {:.2} muted {:06x} look {:06x}", g.tint, g.alpha, g.muted, g.look(bg));
        }
    }

    #[test]
    fn text_between_its_backdrops_does_not_read() {
        assert!(reads_over(0x00ff_ffff, &[0, 0x0020_2020]));
        assert!(reads_over(0, &[0x00ff_ffff, 0x00c0_c0c0]));
        assert!(!reads_over(0x0080_8080, &[0, 0x00ff_ffff]));
        // #767676 reads at 4.5:1 on both black and white, but not on the
        // greys between them.
        assert!(theme::contrast(0x0076_7676, 0) >= theme::TEXT_CONTRAST && theme::contrast(0x0076_7676, 0x00ff_ffff) >= theme::TEXT_CONTRAST);
        assert!(!reads_over(0x0076_7676, &[0x00ff_ffff, 0]));
    }

    #[test]
    fn content_scope_nests() {
        assert!(!inside());
        {
            let _outer = enter();
            {
                let _inner = enter();
                assert!(inside());
            }
            assert!(inside());
        }
        assert!(!inside());
    }

    #[test]
    fn content_on_glass_takes_its_colours() {
        set_available(true);
        let (muted, row) = (theme::muted(), theme::row_fill());
        assert!((row.a - 1.0).abs() < f32::EPSILON, "solid outside");
        let _glass = enter();
        assert_eq!(theme::muted(), current().muted);
        assert_ne!(theme::muted(), muted);
        assert!(theme::row_fill().a < 1.0, "an ink tint on glass");
        set_available(false);
        assert!((theme::row_fill().a - 1.0).abs() < f32::EPSILON, "opaque glass keeps solid fills");
    }

    #[test]
    fn surfaces_blur_only_when_frosted() {
        fn style(mut el: gpui::Div) -> gpui::StyleRefinement {
            el.style().clone()
        }
        set_available(true);
        let frosted = style(gpui::div().glass(8.0));
        assert_eq!(frosted.backdrop_blur, Some(px(BLUR)));
        assert_eq!(frosted.backdrop_saturation, Some(SATURATION));
        assert_eq!(frosted.corner_smoothing, Some(theme::CORNER_SMOOTHING));
        assert_eq!(frosted.box_shadow.as_ref().map(Vec::len), Some(2));
        set_available(false);
        let solid = style(gpui::div().glass_sheet(14.0));
        assert_eq!(solid.backdrop_blur, None);
        assert!(solid.background.is_some());
    }
}
