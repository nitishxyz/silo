//! Where a node and its parts are, in canvas units: a card's header, face, prompt and Run, the port modules
//! floating left of it and the output dot on the right of every node with an output; a frame's title bar;
//! an asset's or entity's face. Sizes for new nodes, a card's height from its content, aspect locks. Card sizes,
//! ports and gaps are the server's ([`silo_proto::studio::size`]), so the cards an agent places are drawn as
//! placed.

pub use silo_proto::studio::size::{
    CARD_FOOTER, CARD_GAP, CARD_HEADER, CARD_W, GAP, OUTPUT_GUTTER, PORT_GAP, PORT_GUTTER, PORT_HIT, PORT_SIZE, PROMPT_LINE, PROMPT_LINES,
    PROMPT_LINES_OPEN, PROMPT_MAX, PROMPT_MIN, PROMPT_OPEN_MAX, PROMPT_PAD, card_height, card_width, face_height, parse_aspect,
};
use silo_proto::studio::size::{estimate_lines, prompt_height, sane};
use silo_proto::studio::{Canvas, EdgeRole, FrameRole, GenKind, GenStatus, Node, NodeBody, Project, StudioMedia};

use super::geom::{Point, Rect, Size};

/// Port modules ([`PORT_SIZE`], [`PORT_GAP`] left of the card): the first one's centre this far below the
/// card's top, then one every [`PORT_STEP`]. Ports and output dots are hit [`PORT_HIT`] around them (canvas
/// units at 100 %; at least [`super::scene::HIT_SLOP_VIEW`] view points).
pub const PORT_TOP: f64 = 22.0;
pub const PORT_STEP: f64 = 36.0;

/// A card resized no narrower (a narrower stored one is drawn as stored).
pub const CARD_MIN_W: f64 = 200.0;
pub const RUN_W: f64 = 64.0;
/// The footer's button when it says Connect provider.
pub const CONNECT_W: f64 = 124.0;
pub const RUN_H: f64 = 24.0;
/// The take stepper (`2/3`) at the header's right: two halves, back and forward.
pub const STEPPER_W: f64 = 64.0;

pub const NOTE_SIZE: Size = Size::new(240.0, 160.0);
pub const NOTE_MIN: Size = Size::new(120.0, 60.0);
pub const FRAME_SIZE: Size = Size::new(640.0, 400.0);
pub const FRAME_MIN: Size = Size::new(160.0, 100.0);
pub const FRAME_TITLE: f64 = 32.0;
/// Room kept inside a frame's edges (snapping offers it).
pub const FRAME_PAD: f64 = 24.0;
pub const MEDIA_W: f64 = 320.0;
pub const MEDIA_MIN: f64 = 48.0;
pub const AUDIO_SIZE: Size = Size::new(320.0, 72.0);
pub const ENTITY_W: f64 = 200.0;
/// A document card ([`Shape::Asset`] of text): the server's size; its title bar (the file name) over the preview.
pub const DOC_SIZE: Size = Size::new(silo_proto::studio::size::DOC_W, silo_proto::studio::size::DOC_H);
pub const DOC_HEADER: f64 = 32.0;
pub const DOC_MIN: Size = Size::new(160.0, 100.0);
/// The entity's name strip under its sheet.
pub const ENTITY_NAME: f64 = 28.0;
/// The aspect used when nothing says one (16:9).
pub const DEFAULT_ASPECT: f64 = 16.0 / 9.0;

/// What a node is, as far as layout and hit testing go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Card(GenKind),
    Note,
    Asset(StudioMedia),
    Entity,
    Frame(FrameRole),
    Unknown,
}

impl Shape {
    #[must_use]
    pub fn of(body: &NodeBody) -> Self {
        match body {
            NodeBody::Gen { makes, .. } => Self::Card(*makes),
            NodeBody::Note { .. } => Self::Note,
            NodeBody::Asset { media, .. } => Self::Asset(*media),
            NodeBody::Entity { .. } => Self::Entity,
            NodeBody::Frame { role, .. } => Self::Frame(*role),
            NodeBody::Unknown => Self::Unknown,
        }
    }

    #[must_use]
    pub fn is_frame(self) -> bool {
        matches!(self, Self::Frame(_))
    }

    /// A document card: a text asset (`script.md`).
    #[must_use]
    pub fn is_document(self) -> bool {
        self == Self::Asset(StudioMedia::Text)
    }

    /// Wirable from (an output dot): assets, entities, cards, notes.
    #[must_use]
    pub fn has_output(self) -> bool {
        matches!(self, Self::Card(_) | Self::Note | Self::Asset(_) | Self::Entity)
    }

    /// Its face is audio (a compact player, not a picture).
    #[must_use]
    pub fn is_audio(self) -> bool {
        matches!(self, Self::Card(GenKind::Speech | GenKind::Sound | GenKind::Music) | Self::Asset(StudioMedia::Audio))
    }

    /// Resizing keeps its aspect: pictures (images, videos, entities) and cards (whose height follows).
    #[must_use]
    pub fn keeps_aspect(self) -> bool {
        matches!(self, Self::Asset(StudioMedia::Image | StudioMedia::Video) | Self::Entity | Self::Card(_))
    }

    #[must_use]
    pub fn min_size(self) -> Size {
        match self {
            Self::Card(_) => Size::new(CARD_MIN_W, CARD_HEADER + CARD_FOOTER + PROMPT_MIN),
            Self::Note => NOTE_MIN,
            Self::Frame(_) => FRAME_MIN,
            Self::Asset(StudioMedia::Audio) => Size::new(160.0, AUDIO_SIZE.h),
            Self::Asset(StudioMedia::Text) => DOC_MIN,
            _ => Size::new(MEDIA_MIN, MEDIA_MIN),
        }
    }
}

/// What a card's footer button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardAction {
    Run,
    Stop,
    /// It failed (its error on the face).
    Retry,
    /// Its run was refused for want of a provider (`error_code` `no_provider`): connect one, then run.
    Connect,
}

impl CardAction {
    #[must_use]
    pub fn width(self) -> f64 {
        if self == Self::Connect { CONNECT_W } else { RUN_W }
    }
}

/// The footer button of a card with `body`.
#[must_use]
pub fn card_action(body: &NodeBody) -> CardAction {
    match body {
        NodeBody::Gen { status: GenStatus::Queued | GenStatus::Running, .. } => CardAction::Stop,
        NodeBody::Gen { status: GenStatus::Failed, error_code, .. } if error_code.as_deref() == Some("no_provider") => CardAction::Connect,
        NodeBody::Gen { status: GenStatus::Failed, .. } => CardAction::Retry,
        _ => CardAction::Run,
    }
}

/// A card's parts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardParts {
    pub header: Rect,
    /// The picked take edge to edge at its own shape (a draft: the effective aspect; audio: the player).
    pub face: Rect,
    pub prompt: Rect,
    pub footer: Rect,
    pub run: Rect,
    /// Back and forward halves of the take stepper, with two takes or more.
    pub stepper: Option<(Rect, Rect)>,
    /// The face's `w / h`.
    pub aspect: f64,
    /// Its prompt shows in full (up to [`PROMPT_LINES_OPEN`]): it is edited, or the single selected card; else
    /// collapsed to [`PROMPT_LINES`] with an ellipsis.
    pub open: bool,
}

/// A node laid out: everything the view draws it with and hit testing needs. `Copy`: drafts translate it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Laid {
    /// Its index in `canvas.nodes`.
    pub index: usize,
    pub shape: Shape,
    pub rect: Rect,
    /// Frames: how many frames it is in (a shot in a scene: 1).
    pub depth: u16,
    /// A card's ports, top to bottom ([`GenKind::ports`]).
    pub ports: &'static [EdgeRole],
    /// Bit `i`: port `i` has a wire.
    pub wired: u16,
    /// Has an output dot.
    pub output: bool,
    pub card: Option<CardParts>,
    /// An asset's or entity's picture (the entity's name strip below it); a document's preview (its title bar
    /// above it).
    pub face: Option<Rect>,
    /// A frame's title bar.
    pub title: Option<Rect>,
}

impl Laid {
    /// Port `i`'s centre.
    #[must_use]
    pub fn port_center(&self, i: usize) -> Point {
        #[allow(clippy::cast_precision_loss)]
        let i = i as f64;
        Point::new(self.rect.x - PORT_GAP - PORT_SIZE / 2.0, self.rect.y + PORT_TOP + i * PORT_STEP)
    }

    #[must_use]
    pub fn port_rect(&self, i: usize) -> Rect {
        Rect::around(self.port_center(i), PORT_SIZE, PORT_SIZE)
    }

    /// The port `role`'s index, when the card has it.
    #[must_use]
    pub fn port_index(&self, role: EdgeRole) -> Option<usize> {
        self.ports.iter().position(|p| *p == role)
    }

    #[must_use]
    pub fn is_wired(&self, i: usize) -> bool {
        i < 16 && self.wired & (1 << i) != 0
    }

    /// The output dot's centre: level with the first port on a card, else halfway down.
    #[must_use]
    pub fn output_center(&self) -> Point {
        let y = if matches!(self.shape, Shape::Card(_)) { self.rect.y + PORT_TOP } else { self.rect.y + self.rect.h / 2.0 };
        Point::new(self.rect.right() + PORT_GAP + PORT_SIZE / 2.0, y)
    }

    #[must_use]
    pub fn output_rect(&self) -> Rect {
        Rect::around(self.output_center(), PORT_SIZE, PORT_SIZE)
    }

    /// Where a wire into port `role` ends (the card's left edge, level with its first port, when it has no
    /// such port).
    #[must_use]
    pub fn input_point(&self, role: EdgeRole) -> Point {
        self.port_index(role).map_or(Point::new(self.rect.x, self.rect.y + PORT_TOP), |i| self.port_center(i))
    }

    /// Its rect with the floating ports and output dot: what it covers on screen.
    #[must_use]
    pub fn outer(&self) -> Rect {
        let mut r = self.rect;
        if !self.ports.is_empty() {
            r = r.union(self.port_rect(0)).union(self.port_rect(self.ports.len() - 1));
        }
        if self.output {
            r = r.union(self.output_rect());
        }
        r
    }

    /// Moved by `d`, parts and all.
    #[must_use]
    pub fn translated(&self, d: Point) -> Self {
        let mut out = *self;
        out.rect = out.rect.translate(d);
        out.face = out.face.map(|r| r.translate(d));
        out.title = out.title.map(|r| r.translate(d));
        out.card = out.card.map(|c| CardParts {
            header: c.header.translate(d),
            face: c.face.translate(d),
            prompt: c.prompt.translate(d),
            footer: c.footer.translate(d),
            run: c.run.translate(d),
            stepper: c.stepper.map(|(a, b)| (a.translate(d), b.translate(d))),
            ..c
        });
        out
    }

    /// A frame's body (below its title bar).
    #[must_use]
    #[allow(dead_code, reason = "board API used by its tests, not by the view yet")]
    pub fn body(&self) -> Rect {
        match self.title {
            Some(t) => Rect::new(self.rect.x, t.bottom(), self.rect.w, (self.rect.h - t.h).max(0.0)),
            None => self.rect,
        }
    }
}

/// `w / h` from pixel dimensions.
#[must_use]
pub fn dims_aspect(w: Option<u32>, h: Option<u32>) -> Option<f64> {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some(f64::from(w) / f64::from(h)),
        _ => None,
    }
}

/// The effective aspect of things made in frame `frame` of `canvas` (§2), [`DEFAULT_ASPECT`] without one.
#[must_use]
pub fn effective_aspect(project: Option<&Project>, canvas: &Canvas, frame: Option<&str>) -> f64 {
    sane(project.and_then(|p| p.format_for(canvas, frame)).and_then(|(_, f)| f.aspect_ratio()).map_or(DEFAULT_ASPECT, |(w, h)| w / h))
}

/// A card's own aspect setting (`options.aspect_ratio`), when it has one that parses.
#[must_use]
pub fn own_aspect(body: &NodeBody) -> Option<f64> {
    let NodeBody::Gen { options, .. } = body else { return None };
    options.get("aspect_ratio").and_then(|v| v.as_str()).and_then(parse_aspect)
}

/// The shape of a card's face (`w / h`): the picked take's own, else the card's aspect setting, else the
/// effective format's (`Project::format_for` on its frame).
#[must_use]
pub fn card_aspect(project: Option<&Project>, canvas: &Canvas, node: &Node) -> f64 {
    let take = node.body.take().and_then(|t| dims_aspect(t.width, t.height));
    take.or_else(|| own_aspect(&node.body)).map_or_else(|| effective_aspect(project, canvas, node.parent.as_deref()), sane)
}

/// The prompt area's height for `text` in a card `w` wide, estimated (the view's measure replaces it), collapsed
/// or `open`.
#[must_use]
pub fn estimate_prompt(text: &str, w: f64, open: bool) -> f64 {
    prompt_height(estimate_lines(text, w), open)
}

/// The prompt area shown for a measured (or estimated) height: collapsed to [`PROMPT_MAX`], open to
/// [`PROMPT_OPEN_MAX`].
#[must_use]
pub fn shown_prompt(height: f64, open: bool) -> f64 {
    height.clamp(PROMPT_MIN, if open { PROMPT_OPEN_MAX } else { PROMPT_MAX })
}

/// A card's parts at `x`, `y`, `w` wide (its height follows), its prompt area `prompt` high as shown
/// ([`shown_prompt`]), its footer button `action`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn card_parts(makes: GenKind, x: f64, y: f64, w: f64, aspect: f64, prompt: f64, takes: usize, action: CardAction, open: bool) -> CardParts {
    let header = Rect::new(x, y, w, CARD_HEADER);
    let face = Rect::new(x, header.bottom(), w, face_height(makes, w, aspect));
    let prompt = Rect::new(x, face.bottom(), w, shown_prompt(prompt, open));
    let footer = Rect::new(x, prompt.bottom(), w, CARD_FOOTER);
    let bw = action.width().min(w - 16.0);
    let run = Rect::new(footer.right() - bw - 8.0, footer.y + (CARD_FOOTER - RUN_H) / 2.0, bw, RUN_H);
    let stepper = (takes > 1).then(|| {
        let s = Rect::new(header.right() - STEPPER_W - 6.0, header.y + 4.0, STEPPER_W, CARD_HEADER - 8.0);
        (Rect::new(s.x, s.y, s.w / 2.0, s.h), Rect::new(s.x + s.w / 2.0, s.y, s.w / 2.0, s.h))
    });
    CardParts { header, face, prompt, footer, run, stepper, aspect: sane(aspect), open }
}

/// What laying a node out needs beyond the node.
#[derive(Clone, Copy, Debug)]
pub struct LayoutInput {
    /// Cards: their face's aspect ([`card_aspect`]).
    pub aspect: f64,
    /// Cards: the prompt's measured height (else [`estimate_prompt`]).
    pub prompt: Option<f64>,
    /// Cards: the prompt shows in full ([`CardParts::open`]); the board decides.
    pub open: bool,
    /// Frames: their nesting depth.
    pub depth: u16,
    /// Bit `i`: port `i` is wired.
    pub wired: u16,
}

/// Node `index` laid out at `rect` (a card: its `x`, `y`, `w`; its height follows its content).
#[must_use]
pub fn lay(index: usize, node: &Node, rect: Rect, input: LayoutInput) -> Laid {
    let shape = Shape::of(&node.body);
    let mut laid = Laid {
        index,
        shape,
        rect,
        depth: input.depth,
        ports: &[],
        wired: input.wired,
        output: shape.has_output(),
        card: None,
        face: None,
        title: None,
    };
    match (&node.body, shape) {
        (NodeBody::Gen { makes, prompt, takes, .. }, _) => {
            // As stored, even below the minimum (an older canvas's narrow cards stay as they are).
            let w = if rect.w >= MEDIA_MIN { rect.w } else { CARD_MIN_W };
            let p = input.prompt.unwrap_or_else(|| estimate_prompt(prompt, w, input.open));
            let parts = card_parts(*makes, rect.x, rect.y, w, input.aspect, p, takes.len(), card_action(&node.body), input.open);
            laid.rect = Rect::new(rect.x, rect.y, w, parts.footer.bottom() - rect.y);
            laid.ports = makes.ports();
            laid.card = Some(parts);
        }
        (NodeBody::Asset { .. }, Shape::Asset(StudioMedia::Text)) => {
            let bar = DOC_HEADER.min(rect.h);
            laid.face = Some(Rect::new(rect.x, rect.y + bar, rect.w, rect.h - bar));
        }
        (NodeBody::Asset { .. }, _) => laid.face = Some(rect),
        (NodeBody::Entity { .. }, _) => laid.face = Some(Rect::new(rect.x, rect.y, rect.w, (rect.h - ENTITY_NAME).max(0.0))),
        (NodeBody::Frame { .. }, _) => laid.title = Some(Rect::new(rect.x, rect.y, rect.w, FRAME_TITLE.min(rect.h))),
        _ => {}
    }
    laid
}

/// The size of a new node of `body` whose picture or face is `aspect` (`w / h`).
#[must_use]
pub fn new_size(body: &NodeBody, aspect: f64) -> Size {
    match body {
        NodeBody::Gen { makes, prompt, .. } => {
            let w = card_width(*makes, aspect);
            Size::new(w, card_height(*makes, w, aspect, estimate_lines(prompt, w)))
        }
        NodeBody::Note { .. } => NOTE_SIZE,
        NodeBody::Frame { .. } => FRAME_SIZE,
        NodeBody::Asset { media: StudioMedia::Audio, .. } => AUDIO_SIZE,
        NodeBody::Asset { media: StudioMedia::Text, .. } => DOC_SIZE,
        NodeBody::Asset { width, height, .. } => {
            let a = sane(dims_aspect(*width, *height).unwrap_or(aspect));
            Size::new(MEDIA_W, (MEDIA_W / a).round())
        }
        NodeBody::Entity { .. } => Size::new(ENTITY_W, (ENTITY_W / sane(aspect)).round() + ENTITY_NAME),
        NodeBody::Unknown => NOTE_SIZE,
    }
}

/// The aspect a resize of `laid` keeps (`w / h` of its rect, or of its face for entities and cards).
#[must_use]
pub fn locked_aspect(laid: &Laid) -> Option<f64> {
    if !laid.shape.keeps_aspect() {
        return None;
    }
    let r = laid.rect;
    match laid.shape {
        Shape::Card(_) => laid.card.map(|c| c.aspect),
        Shape::Entity => Some((r.w / (r.h - ENTITY_NAME).max(1.0)).max(0.01)),
        _ => Some((r.w / r.h.max(1.0)).max(0.01)),
    }
}
