//! Wires: the curve from an output dot to a port, its bounds and distance for hit testing, and what dropping
//! a wire being dragged would do (a wire, a refusal with its reason, or the drag-to-create menu).

use silo_proto::studio::{Canvas, EdgeRole, GenKind, NodeBody, SourceMedia, default_port};

use super::geom::{Point, Rect, dist_to_segment};
use super::scene::{Hit, Part};

/// A cubic Bézier: `a` to `b` through controls `c1`, `c2`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Curve {
    pub a: Point,
    pub c1: Point,
    pub c2: Point,
    pub b: Point,
}

/// Segments a curve is measured with.
const SAMPLES: usize = 24;

impl Curve {
    /// A wire from output `a` to input `b`: a horizontal S, looping round when the input is behind.
    #[must_use]
    pub fn wire(a: Point, b: Point) -> Self {
        let dx = b.x - a.x;
        let bend = (dx.abs() / 2.0 + if dx < 0.0 { 80.0 } else { 0.0 }).clamp(60.0, 220.0);
        Self { a, c1: Point::new(a.x + bend, a.y), c2: Point::new(b.x - bend, b.y), b }
    }

    #[must_use]
    pub fn point(&self, t: f64) -> Point {
        let u = 1.0 - t;
        self.a * (u * u * u) + self.c1 * (3.0 * u * u * t) + self.c2 * (3.0 * u * t * t) + self.b * (t * t * t)
    }

    /// Its middle (`t` = 0.5), for a label or a menu.
    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn mid(&self) -> Point {
        self.point(0.5)
    }

    /// Bounds of its control points (they hold the curve).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::from_points(self.a, self.b).union(Rect::from_points(self.c1, self.c2))
    }

    /// The closest distance from `p` to it (on [`SAMPLES`] segments).
    #[must_use]
    pub fn distance(&self, p: Point) -> f64 {
        let mut prev = self.a;
        let mut best = f64::INFINITY;
        for k in 1..=SAMPLES {
            #[allow(clippy::cast_precision_loss)]
            let next = self.point(k as f64 / SAMPLES as f64);
            best = best.min(dist_to_segment(p, prev, next));
            prev = next;
        }
        best
    }

    /// `n` + 1 points along it, for drawing it as lines.
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn points(&self, n: usize) -> impl Iterator<Item = Point> + '_ {
        let n = n.max(1);
        #[allow(clippy::cast_precision_loss)]
        (0..=n).map(move |k| self.point(k as f64 / n as f64))
    }
}

/// Where a wire being dragged started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireEnd {
    /// From node `from`'s output dot, looking for a port.
    Out { from: String },
    /// From card `to`'s `port`, looking for a source.
    In { to: String, port: EdgeRole },
}

/// What dropping the wire being dragged would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireDrop {
    /// Wire `from` into `to`'s `port`; `replaces`: the wire a frame port held.
    Wire { from: String, to: String, port: EdgeRole, replaces: Vec<String> },
    /// Over node `node`, which can't take it: why (the port doesn't exist, the wrong media, a loop, …).
    Refused { node: String, port: Option<EdgeRole>, reason: String },
    /// Over empty canvas: the drag-to-create menu ([`create_options`]).
    Create,
    /// Back where it started: nothing.
    Nothing,
}

impl WireDrop {
    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn ok(&self) -> bool {
        matches!(self, Self::Wire { .. } | Self::Create)
    }
}

/// The ports card `id` already has wires in.
#[must_use]
pub fn taken_ports(canvas: &Canvas, id: &str) -> Vec<EdgeRole> {
    canvas.edges.iter().filter(|e| e.to == id && e.role.is_port()).map(|e| e.role).collect()
}

fn check(canvas: &Canvas, from: &str, to: &str, port: EdgeRole, at: &str) -> WireDrop {
    match canvas.wire_check(from, to, port) {
        Ok(replaces) => WireDrop::Wire { from: from.to_owned(), to: to.to_owned(), port, replaces },
        Err(reason) => WireDrop::Refused { node: at.to_owned(), port: Some(port), reason: capitalize(&reason) },
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// What dropping a wire from `end` on `hit` does. Onto a port: that port (refused when the card lacks it, the
/// media is wrong or it would loop; a frame port's wire is replaced). Onto a card's body: its first free port
/// that fits ([`default_port`]). Onto empty canvas or a frame's body: the drag-to-create menu.
#[must_use]
pub fn resolve(canvas: &Canvas, end: &WireEnd, hit: &Hit) -> WireDrop {
    match end {
        WireEnd::Out { from } => match hit {
            Hit::Port { id, .. } | Hit::Node { id, .. } | Hit::Output { id } | Hit::Resize { id, .. } if id == from => WireDrop::Nothing,
            Hit::Port { id, port } => check(canvas, from, id, *port, id),
            Hit::Node { part: Part::FrameBody, .. } | Hit::Empty | Hit::Wire { .. } => WireDrop::Create,
            Hit::Node { id, .. } | Hit::Output { id } | Hit::Resize { id, .. } => {
                let Some(target) = canvas.node(id) else { return WireDrop::Nothing };
                let NodeBody::Gen { makes, .. } = &target.body else {
                    return WireDrop::Refused { node: id.clone(), port: None, reason: "Wires go into cards".into() };
                };
                let Some(media) = canvas.source_media(from) else { return WireDrop::Nothing };
                match default_port(*makes, media, &taken_ports(canvas, id)) {
                    Some(port) => check(canvas, from, id, port, id),
                    None => {
                        let fits = makes.ports().iter().any(|p| p.accepts() == Some(media));
                        let kind = makes.label().to_lowercase();
                        let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) { "An" } else { "A" };
                        let reason = if fits {
                            format!("This {kind} card has no free port for {}: drop it on a port", media.name())
                        } else {
                            format!("{article} {kind} card can't take {}", media.name())
                        };
                        WireDrop::Refused { node: id.clone(), port: None, reason }
                    }
                }
            }
        },
        WireEnd::In { to, port } => match hit {
            Hit::Port { id, .. } | Hit::Node { id, .. } | Hit::Output { id } | Hit::Resize { id, .. } if id == to => WireDrop::Nothing,
            Hit::Node { part: Part::FrameBody, .. } | Hit::Empty | Hit::Wire { .. } => WireDrop::Create,
            Hit::Port { id, .. } => WireDrop::Refused { node: id.clone(), port: None, reason: "Drop it on what feeds the card: an output".into() },
            Hit::Node { id, .. } | Hit::Output { id } | Hit::Resize { id, .. } => {
                if canvas.source_media(id).is_none() {
                    return WireDrop::Refused { node: id.clone(), port: None, reason: "This has no output to wire".into() };
                }
                check(canvas, id, to, *port, id)
            }
        },
    }
}

/// An entry of the drag-to-create menu: a card of kind `makes` (`None`: a note) wired through `port`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateOption {
    pub makes: Option<GenKind>,
    pub port: EdgeRole,
}

/// The card kinds, in menu order.
pub const KINDS: [GenKind; 5] = [GenKind::Image, GenKind::Video, GenKind::Speech, GenKind::Sound, GenKind::Music];

/// A kind's name in menus and the canvas prompt (speech reads "Voice").
#[must_use]
pub fn kind_label(makes: GenKind) -> &'static str {
    match makes {
        GenKind::Speech => "Voice",
        other => other.label(),
    }
}

/// What the drag-to-create menu offers for a wire from `end`: from a source, the cards with a port for its
/// media (an image: Image through `reference`, Video through `first_frame`; a video: Video; a note: any); from
/// a port, the cards whose output it takes (and a note into `text`).
#[must_use]
pub fn create_options(canvas: &Canvas, end: &WireEnd) -> Vec<CreateOption> {
    match end {
        WireEnd::Out { from } => {
            let Some(media) = canvas.source_media(from) else { return Vec::new() };
            KINDS.iter().filter_map(|&k| default_port(k, media, &[]).map(|port| CreateOption { makes: Some(k), port })).collect()
        }
        WireEnd::In { port, .. } => {
            let mut out: Vec<CreateOption> =
                KINDS.iter().filter(|k| port.accepts() == Some(k.media())).map(|&k| CreateOption { makes: Some(k), port: *port }).collect();
            if port.accepts() == Some(SourceMedia::Text) {
                out.push(CreateOption { makes: None, port: *port });
            }
            out
        }
    }
}
