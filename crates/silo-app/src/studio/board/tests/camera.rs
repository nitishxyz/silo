//! Pan and zoom, canvas ↔ view, fitting, the visible rect and the minimap.

use silo_proto::studio::GenKind;

use super::super::camera::{FIT_PADDING, MAX_ZOOM, MIN_ZOOM, clamp_zoom};
use super::super::{Board, Button, Camera, Event, Minimap, Point, Rect, Size};
use super::fixtures::*;

fn cam() -> Camera {
    Camera { pan: Point::new(130.0, -70.0), zoom: 1.7 }
}

#[test]
fn canvas_and_view_round_trip() {
    let c = cam();
    for p in [Point::ZERO, Point::new(12.5, -400.0), Point::new(-3000.0, 2200.0)] {
        close_pt(c.to_canvas(c.to_view(p)), p);
        close_pt(c.to_view(c.to_canvas(p)), p);
    }
    let r = Rect::new(10.0, 20.0, 300.0, 40.0);
    let back = c.rect_to_canvas(c.rect_to_view(r));
    close_pt(back.origin(), r.origin());
    close(back.w, r.w);
    close(back.h, r.h);
    // A canvas point shows at p * zoom + pan.
    close_pt(c.to_view(Point::new(10.0, 10.0)), Point::new(147.0, -53.0));
}

#[test]
fn zoom_keeps_the_point_under_the_cursor() {
    let c = cam();
    let at = Point::new(400.0, 300.0);
    let under = c.to_canvas(at);
    for z in [0.1, 0.5, 2.5, 4.0] {
        let zc = c.zoomed_at(at, z);
        close(zc.zoom, z);
        close_pt(zc.to_view(under), at);
    }
}

#[test]
fn zoom_is_clamped_to_5_and_400_percent() {
    let at = Point::new(10.0, 10.0);
    close(cam().zoomed_at(at, 100.0).zoom, MAX_ZOOM);
    close(cam().zoomed_at(at, 0.0001).zoom, MIN_ZOOM);
    close(MIN_ZOOM, 0.05);
    close(MAX_ZOOM, 4.0);
    close(clamp_zoom(f64::NAN), 1.0);

    let mut b = board(&canvas(vec![note("a", 0.0, 0.0, "")], vec![]));
    b.handle(Event::Pinch { at, factor: 1000.0 });
    close(b.camera().zoom, 4.0);
    b.handle(Event::Scroll { at, delta: Point::new(0.0, 100_000.0), mods: CMD });
    close(b.camera().zoom, 0.05);
    let _ = b.set_camera(Camera { pan: Point::ZERO, zoom: 50.0 });
    close(b.camera().zoom, 4.0);
}

#[test]
fn scroll_pans_and_pinch_or_cmd_scroll_zoom_around_the_pointer() {
    let mut b = board(&canvas(vec![note("a", 0.0, 0.0, "")], vec![]));
    b.handle(Event::Scroll { at: Point::new(5.0, 5.0), delta: Point::new(30.0, -20.0), mods: NONE });
    assert_eq!(b.camera(), Camera { pan: Point::new(30.0, -20.0), zoom: 1.0 });

    let at = Point::new(500.0, 300.0);
    let under = b.camera().to_canvas(at);
    b.handle(Event::Pinch { at, factor: 2.0 });
    close(b.camera().zoom, 2.0);
    close_pt(b.camera().to_view(under), at);

    // ⌘-scroll: up (negative y) zooms in, around the pointer.
    let z = b.camera().zoom;
    b.handle(Event::Scroll { at, delta: Point::new(0.0, -50.0), mods: CMD });
    assert!(b.camera().zoom > z);
    close_pt(b.camera().to_view(under), at);
}

#[test]
fn fit_shows_the_rect_centred_with_padding_never_past_100_percent() {
    let size = Size::new(1000.0, 600.0);
    let big = Rect::new(-500.0, 200.0, 4000.0, 1000.0);
    let c = Camera::fit(big, size, FIT_PADDING);
    let v = c.rect_to_view(big);
    assert!(c.zoom < 1.0);
    assert!(v.x >= FIT_PADDING - 1e-6 && v.right() <= size.w - FIT_PADDING + 1e-6, "{v:?}");
    assert!(v.y >= FIT_PADDING - 1e-6 && v.bottom() <= size.h - FIT_PADDING + 1e-6, "{v:?}");
    close_pt(v.center(), Point::new(500.0, 300.0));

    let small = Rect::new(100.0, 100.0, 50.0, 20.0);
    let c = Camera::fit(small, size, FIT_PADDING);
    close(c.zoom, 1.0);
    close_pt(c.to_view(small.center()), Point::new(500.0, 300.0));
}

#[test]
fn fit_all_and_fit_selection() {
    let c = canvas(vec![note("a", -2000.0, -1000.0, ""), note("b", 3000.0, 1500.0, ""), note("c", 10.0, 10.0, "")], vec![]);
    let mut b = board(&c);
    let _ = b.set_camera(Camera { pan: Point::new(9999.0, 9999.0), zoom: 3.0 });
    let fx = ch(&mut b, '0', CMD);
    assert!(!fx.is_empty());
    let content = b.scene().content.unwrap();
    let vis = b.visible_rect();
    assert!(vis.contains_rect(content), "{vis:?} should hold {content:?}");

    // `f` with a selection fits it (100 % at most), centred.
    b.select(&["c".to_owned()]);
    ch(&mut b, 'f', NONE);
    close(b.camera().zoom, 1.0);
    let l = b.laid("c").unwrap();
    close_pt(b.camera().to_view(l.outer().center()), Point::new(VIEW.w / 2.0, VIEW.h / 2.0));
}

#[test]
fn the_first_canvas_is_fitted_once_its_size_is_known() {
    let c = canvas(vec![note("a", 5000.0, 5000.0, "")], vec![]);
    let mut b = Board::new(1);
    b.set_canvas(&c);
    assert_eq!(b.camera(), Camera::default());
    b.set_view_size(VIEW);
    assert!(b.visible_rect().contains_rect(b.laid("a").unwrap().rect));
    // Later reads don't refit.
    let _ = b.set_camera(Camera::default());
    let mut c2 = c.clone();
    c2.rev = 2;
    b.set_canvas(&c2);
    assert_eq!(b.camera(), Camera::default());
    // Another canvas does.
    let mut other = c.clone();
    other.name = "ep02".into();
    b.set_canvas(&other);
    assert_ne!(b.camera(), Camera::default());
}

#[test]
fn visible_rect() {
    let c = Camera { pan: Point::new(100.0, 50.0), zoom: 2.0 };
    assert_eq!(c.visible(Size::new(800.0, 600.0)), Rect::new(-50.0, -25.0, 400.0, 300.0));
    let mut b = board(&canvas(vec![], vec![]));
    let _ = b.set_camera(c);
    assert_eq!(b.visible_rect(), Rect::new(-50.0, -25.0, 600.0, 400.0));
}

#[test]
fn reveal_pans_the_least_needed() {
    let size = Size::new(800.0, 600.0);
    let c = Camera::default();
    let inside = Rect::new(100.0, 100.0, 100.0, 100.0);
    assert_eq!(c.reveal(inside, size, 20.0), c);
    let right = Rect::new(1000.0, 100.0, 100.0, 100.0);
    let r = c.reveal(right, size, 20.0);
    close(r.pan.y, 0.0);
    close(r.rect_to_view(right).right(), 780.0);
}

#[test]
fn minimap_geometry() {
    let frame = Rect::new(1000.0, 600.0, 180.0, 120.0);
    let visible = Rect::new(0.0, 0.0, 1200.0, 800.0);
    // Everything in view: none.
    assert!(Minimap::new(frame, Some(Rect::new(10.0, 10.0, 100.0, 100.0)), visible).is_none());
    assert!(Minimap::new(frame, None, visible).is_none());

    let content = Rect::new(-1000.0, -200.0, 4000.0, 1200.0);
    let m = Minimap::new(frame, Some(content), visible).unwrap();
    // The content and the view both fit inside the frame, centred.
    let mc = m.rect(content);
    assert!(frame.contains_rect(mc), "{mc:?} in {frame:?}");
    assert!(frame.contains_rect(m.rect(visible)));
    let world = m.rect(m.world);
    close_pt(world.center(), frame.center());
    // A minimap point maps back to the canvas point drawn there.
    let p = Point::new(700.0, 300.0);
    let shown = m.rect(Rect::new(p.x, p.y, 0.0, 0.0)).origin();
    close_pt(m.to_canvas(shown), p);
    // Pressing there centres the camera on it.
    let cam = m.camera_at(Camera { pan: Point::ZERO, zoom: 0.5 }, shown, visible.size());
    close(cam.zoom, 0.5);
    close_pt(cam.to_view(p), visible.center());
}

#[test]
fn a_press_on_the_minimap_moves_the_camera() {
    let c = canvas(vec![card("a", GenKind::Image, -3000.0, 0.0), note("b", 3000.0, 2000.0, "")], vec![]);
    let mut b = board(&c);
    let frame = Rect::new(1000.0, 650.0, 180.0, 130.0);
    b.set_minimap_frame(Some(frame));
    let m = b.minimap(frame).expect("content larger than the view");
    let target = m.rect(b.laid("b").unwrap().rect).center();
    b.handle(Event::Down { at: target, button: Button::Left, mods: NONE, clicks: 1 });
    b.handle(Event::Up { at: target, button: Button::Left, mods: NONE });
    let centre = b.camera().to_canvas(Point::new(VIEW.w / 2.0, VIEW.h / 2.0));
    close_pt(centre, m.to_canvas(target));
    // Nothing was selected or sent.
    assert!(b.selection().is_empty());
}
