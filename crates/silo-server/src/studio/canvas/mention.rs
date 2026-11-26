//! Mentions in cards' prompts as wires (docs/STUDIO.md §3, "Mentions"). After an edit's ops, every card whose
//! prompt they set or changed is compared with its prompt before: a token written wires its source into the card
//! (an Entity node is added for an entity the canvas doesn't show yet, an Asset node for a named reference's file)
//! unless that source is already wired into it at some port; a token removed takes away the wire it had made
//! (`Edge::mention`), never one drawn by hand. Part of the same write: one rev, one undo step.

use std::collections::{HashMap, HashSet};

use silo_proto::studio::{
    Canvas, Edge, FrameRole, GenKind, MentionDid, MentionSynced, NewNode, NodeBody, Section, SourceMedia, StudioMedia,
    mention::{self, Target},
};

use super::{EPS, Edit, GAP, PAD, aspect_in, extent, gap_between, is_card, new_id, right_gutter, size_for, world};

/// The cards' prompts by id: what [`Edit::sync_mentions`] compares an edit's result with.
pub(super) fn prompts(canvas: &Canvas) -> HashMap<String, String> {
    canvas
        .nodes
        .iter()
        .filter_map(|n| match &n.body {
            NodeBody::Gen { prompt, .. } => Some((n.id.clone(), prompt.clone())),
            _ => None,
        })
        .collect()
}

/// The distinct targets of `text`'s tokens, in order.
fn targets(text: &str) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    for t in mention::tokens(text) {
        if !out.contains(&t.target) {
            out.push(t.target);
        }
    }
    out
}

/// `a`/`an` and the card kind in lower case.
fn a_card(makes: GenKind) -> String {
    let kind = makes.label().to_lowercase();
    let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    format!("{article} {kind} card")
}

impl Edit<'_> {
    /// Wires and unwires for the mentions of every card whose prompt differs from `before` (a card not in it is
    /// new); what it did, in card order.
    pub(super) fn sync_mentions(&mut self, before: &HashMap<String, String>) -> Vec<MentionSynced> {
        let changed: Vec<(String, GenKind, String)> = self
            .doc
            .canvas
            .nodes
            .iter()
            .filter_map(|n| match &n.body {
                NodeBody::Gen { makes, prompt, .. } if before.get(&n.id) != Some(prompt) => Some((n.id.clone(), *makes, prompt.clone())),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        let mut sections: Option<Vec<Section>> = None;
        for (card, makes, prompt) in changed {
            let old = targets(before.get(&card).map_or("", String::as_str));
            let now = targets(&prompt);
            // Which file a reference token names is the world's to say.
            let refs = old.iter().chain(&now).any(|t| matches!(t, Target::Ref { .. }));
            let world = if refs { sections.get_or_insert_with(|| world::sections(self.root)).as_slice() } else { &[] };
            self.unwire(&card, &old, &now, world, &mut out);
            let added: Vec<Target> = now.into_iter().filter(|t| !old.contains(t)).collect();
            if added.is_empty() {
                continue;
            }
            let sections = sections.get_or_insert_with(|| world::sections(self.root));
            for target in added {
                if let Some(done) = self.wire(&card, makes, &target, sections) {
                    out.push(done);
                }
            }
        }
        out
    }

    /// Removes the wires mentions made into `card` whose source no token in `now` names.
    fn unwire(&mut self, card: &str, old: &[Target], now: &[Target], sections: &[Section], out: &mut Vec<MentionSynced>) {
        let canvas = &self.doc.canvas;
        let gone: Vec<Edge> = canvas
            .edges
            .iter()
            .filter(|e| e.to == card && e.mention && !now.iter().any(|t| canvas.mention_names(t, &e.from, sections)))
            .cloned()
            .collect();
        let ids: HashSet<String> = gone.iter().map(|e| e.id.clone()).collect();
        for edge in gone {
            let token = old.iter().find(|t| canvas.mention_names(t, &edge.from, sections)).cloned().unwrap_or_else(|| {
                match canvas.node(&edge.from).map(|n| &n.body) {
                    Some(NodeBody::Entity { id }) => Target::Entity(id.clone()),
                    _ => Target::Node(edge.from.clone()),
                }
            });
            out.push(MentionSynced {
                card: card.to_owned(),
                token: token.token(),
                did: MentionDid::Unwired,
                edge: Some(edge),
                node: None,
                why: None,
            });
        }
        self.doc.canvas.edges.retain(|e| !ids.contains(&e.id));
    }

    /// Wires `target`'s source into `card` (`makes`), unless it doesn't resolve (nothing to report) or that
    /// source already feeds the card at any port (nothing to do).
    fn wire(&mut self, card: &str, makes: GenKind, target: &Target, sections: &[Section]) -> Option<MentionSynced> {
        let canvas = &self.doc.canvas;
        let label = canvas.mention_label(target, card, sections)?;
        let sources: Vec<String> = canvas.mention_sources(target, card, sections).into_iter().map(str::to_owned).collect();
        if sources.iter().any(|s| canvas.edges.iter().any(|e| &e.from == s && e.to == card && e.role.is_port())) {
            return None;
        }
        let report = |did, edge, node, why| MentionSynced { card: card.to_owned(), token: target.token(), did, edge, node, why };
        // An entity with no node on the canvas yet outputs its sheet; a reference its file.
        let look = target.look(sections).map(|(_, look)| look);
        let unplaced = match look.map(|l| l.media) {
            None | Some(StudioMedia::Image) => SourceMedia::Image,
            Some(StudioMedia::Video) => SourceMedia::Video,
            Some(StudioMedia::Audio) => SourceMedia::Audio,
            Some(StudioMedia::Text) => SourceMedia::Text,
            Some(StudioMedia::Unknown) => SourceMedia::Unknown,
        };
        let media = sources.first().map_or(Some(unplaced), |s| canvas.source_media(s)).unwrap_or(SourceMedia::Unknown);
        let Some(role) = mention::port_for(media) else {
            let why = format!("{label} outputs {}: wire it by hand into the port it is for", media.name());
            return Some(report(MentionDid::NotWired, None, None, Some(why)));
        };
        if !makes.ports().contains(&role) {
            let why = format!("{} has no {} port for {label}", a_card(makes), role.name());
            return Some(report(MentionDid::NotWired, None, None, Some(why)));
        }
        let (from, added) = match sources.into_iter().next() {
            Some(s) => (s, None),
            None => {
                let body = match (target, look) {
                    (Target::Entity(id), _) => NodeBody::Entity { id: id.clone() },
                    (Target::Ref { .. }, Some(look)) => {
                        NodeBody::Asset { path: look.path.clone(), media: look.media, width: None, height: None, seconds: None, prompt: None }
                    }
                    _ => return None,
                };
                let parent = self.home_of(card);
                // Upstream of the card when there is room; else where any new node goes.
                let spot = self.left_of(card, parent.as_deref(), &body);
                match self.add_node(NewNode { id: None, x: spot.map(|s| s.0), y: spot.map(|s| s.1), w: None, h: None, z: None, parent, body }) {
                    Ok(node) => (node.clone(), Some(node)),
                    Err(e) => return Some(report(MentionDid::NotWired, None, None, Some(e.message))),
                }
            }
        };
        let canvas = &self.doc.canvas;
        match canvas.wire_check(&from, card, role) {
            Err(why) => Some(report(MentionDid::NotWired, None, added, Some(why))),
            Ok(replaced) => {
                // Not a frame port: nothing is replaced (kept general all the same).
                let id = new_id("e_", |i| canvas.edges.iter().any(|e| e.id == i) || canvas.nodes.iter().any(|n| n.id == i));
                let edge = Edge { id, from, to: card.to_owned(), role, label: None, mention: true };
                let edges = &mut self.doc.canvas.edges;
                edges.retain(|e| !replaced.contains(&e.id));
                edges.push(edge.clone());
                Some(report(MentionDid::Wired, Some(edge), added, None))
            }
        }
    }

    /// A free place for a source of `card` left of it (left of its shot frame when the source goes beside that,
    /// in `home`), in a column going down from the card's top past what is already there: wires run left to
    /// right. `None` without room (the frame's edges, other nodes all the way down): the usual placement.
    fn left_of(&self, card: &str, home: Option<&str>, body: &NodeBody) -> Option<(f64, f64)> {
        const TRIES: usize = 6;
        let canvas = &self.doc.canvas;
        let node = canvas.node(card)?;
        let anchor = if node.parent.as_deref() == home { node } else { canvas.node(node.parent.as_deref()?)? };
        let (w, h) = size_for(body, aspect_in(self.project, canvas, home));
        let [left, top, ..] = extent(anchor);
        let gutter = right_gutter(body);
        let x = left - gap_between(body, is_card(&anchor.body)) - gutter - w;
        let frame = home.and_then(|p| canvas.node(p));
        if frame.is_some_and(|f| x < f.x + PAD) {
            return None;
        }
        (0..TRIES).map(|k| top + k as f64 * (h + GAP)).take_while(|y| frame.is_none_or(|f| y + h <= f.y + f.h - PAD)).find_map(|y| {
            let clear = canvas.nodes.iter().filter(|n| n.parent.as_deref() == home).all(|n| {
                let e = extent(n);
                e[2] <= x + EPS || e[0] >= x + w + gutter - EPS || e[3] <= y + EPS || e[1] >= y + h - EPS
            });
            clear.then_some((x, y))
        })
    }

    /// Where an Entity node for `card` goes: the card's frame, but beside a shot frame (in its scene, or at the
    /// top level) rather than in it, as the canvas keeps a scene's Entity nodes outside its shots.
    fn home_of(&self, card: &str) -> Option<String> {
        let canvas = &self.doc.canvas;
        let parent = canvas.node(card)?.parent.clone()?;
        match canvas.node(&parent).map(|n| &n.body) {
            Some(NodeBody::Frame { role: FrameRole::Shot, .. }) => {
                canvas.node(&parent).and_then(|n| n.parent.clone()).filter(|p| canvas.node(p).is_some())
            }
            Some(NodeBody::Frame { .. }) => Some(parent),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use silo_proto::studio::{CanvasEdited, EdgeRole, MentionDid, NodeBody, Op};

    use super::super::{create, edit, load};
    use crate::studio::project;

    struct Studio(PathBuf);

    impl Drop for Studio {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A studio with Mara (`characters/mara`) and the harbor, and canvas `c`.
    fn studio(name: &str) -> Studio {
        let root = std::env::temp_dir().join(format!("silo-mentions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        project::init(&root, "T", None).unwrap();
        for (dir, name) in [("characters/mara", "Mara Venn"), ("places/harbor", "The Harbor")] {
            let leaf = dir.rsplit('/').next().unwrap();
            std::fs::create_dir_all(root.join("world").join(dir)).unwrap();
            std::fs::write(root.join("world").join(dir).join(format!("{leaf}.md")), format!("---\nname: {name}\n---\n")).unwrap();
        }
        create(&root, "C", None, None, Some("c")).unwrap();
        Studio(root)
    }

    fn run(root: &Path, ops: Value) -> CanvasEdited {
        let ops: Vec<Op> = serde_json::from_value(ops).unwrap();
        edit(root, "c", ops, None).unwrap()
    }

    fn canvas(root: &Path) -> silo_proto::studio::Canvas {
        load(root, "c").unwrap().canvas
    }

    fn card(id: &str, makes: &str, prompt: &str) -> Value {
        json!({"op": "add_node", "node": {"id": id, "body": {"kind": "gen", "makes": makes, "prompt": prompt}}})
    }

    fn prompt(id: &str, prompt: &str) -> Value {
        json!({"op": "update_node", "id": id, "body": {"prompt": prompt}})
    }

    fn node(id: &str, body: Value) -> Value {
        json!({"op": "add_node", "node": {"id": id, "body": body}})
    }

    /// The wires into `to`: source, port, made by a mention.
    fn wires(root: &Path, to: &str) -> Vec<(String, EdgeRole, bool)> {
        canvas(root).edges.iter().filter(|e| e.to == to).map(|e| (e.from.clone(), e.role, e.mention)).collect()
    }

    fn entity_nodes(root: &Path, id: &str) -> Vec<String> {
        canvas(root).nodes.iter().filter(|n| matches!(&n.body, NodeBody::Entity { id: e } if e == id)).map(|n| n.id.clone()).collect()
    }

    #[test]
    fn an_entity_token_adds_its_node_beside_the_shot_and_wires_it() {
        let s = studio("entity");
        let root = &s.0;
        run(
            root,
            json!([
                node("scene", json!({"kind": "frame", "role": "scene", "title": "S1"})),
                {"op": "add_node", "node": {"id": "shot", "parent": "scene", "body": {"kind": "frame", "role": "shot", "title": "1"}}},
            ]),
        );
        let rev = canvas(root).rev;
        let mut add = card("key", "image", "@characters/mara walks the quay with @places/harbor behind, mail mara@harbor.com");
        add["node"]["parent"] = json!("shot");
        let done = run(root, json!([add]));
        assert_eq!(done.rev, rev + 1, "one write");
        assert_eq!(done.mentions.len(), 2);
        assert!(done.mentions.iter().all(|m| m.did == MentionDid::Wired && m.card == "key" && m.node.is_some()));
        assert_eq!(done.mentions[0].token, "@characters/mara");
        let c = canvas(root);
        let mara = entity_nodes(root, "characters/mara");
        assert_eq!(mara.len(), 1);
        assert_eq!(c.node(&mara[0]).unwrap().parent.as_deref(), Some("scene"), "in the scene, outside the shot");
        let harbor = entity_nodes(root, "places/harbor");
        assert_eq!(wires(root, "key"), [(mara[0].clone(), EdgeRole::Reference, true), (harbor[0].clone(), EdgeRole::Reference, true)]);
        assert_eq!(done.mentions[0].edge.as_ref().map(|e| e.mention), Some(true));
        // No overlap with the shot frame: placed by the usual rules (the scene has no room left of its shot).
        let (m, f) = (c.node(&mara[0]).unwrap(), c.node("shot").unwrap());
        assert!(m.x >= f.x + f.w || m.y >= f.y + f.h || m.x + m.w <= f.x || m.y + m.h <= f.y);
    }

    #[test]
    fn an_added_entity_goes_left_of_its_card_when_there_is_room() {
        let s = studio("left");
        let root = &s.0;
        run(root, json!([card("key", "image", "The quay"), card("clip", "video", "She turns")]));
        run(root, json!([prompt("clip", "@characters/mara turns")]));
        let c = canvas(root);
        let (key, clip) = (c.node("key").unwrap(), c.node("clip").unwrap());
        // No room between the two cards: left of the second one all the same, under the first.
        let mara = c.node(&entity_nodes(root, "characters/mara")[0]).unwrap();
        assert!(mara.x + mara.w < clip.x && mara.y >= key.y + key.h, "{mara:?}");
        // Left of the first card: level with its top, the second source under the first.
        run(root, json!([prompt("key", "@places/harbor at dawn, @characters/mara small")]));
        let c = canvas(root);
        assert_eq!(entity_nodes(root, "characters/mara").len(), 1, "the node there is reused");
        let harbor = c.node(&entity_nodes(root, "places/harbor")[0]).unwrap();
        assert!(harbor.x + harbor.w < key.x && harbor.y == key.y, "{harbor:?} {key:?}");
        run(root, json!([{"op": "remove_node", "id": mara.id}, prompt("key", "@places/harbor at dawn"), prompt("clip", "She turns")]));
        run(root, json!([prompt("key", "@places/harbor, @characters/mara")]));
        let c = canvas(root);
        let again = c.node(&entity_nodes(root, "characters/mara")[0]).unwrap();
        assert!(again.x == harbor.x && again.y >= harbor.y + harbor.h, "stacked under the harbor: {again:?}");
    }

    #[test]
    fn an_existing_entity_node_or_hand_wire_is_reused() {
        let s = studio("reuse");
        let root = &s.0;
        run(
            root,
            json!([node("m", json!({"kind": "entity", "id": "characters/mara"})), node("h", json!({"kind": "entity", "id": "places/harbor"}))]),
        );
        run(root, json!([card("key", "image", "x"), {"op": "add_edge", "edge": {"from": "h", "to": "key", "role": "reference"}}]));
        let done = run(root, json!([prompt("key", "@characters/mara at @places/harbor")]));
        assert_eq!(done.mentions.len(), 1, "the harbor was wired by hand already");
        assert_eq!(done.mentions[0].node, None, "Mara's node reused");
        assert_eq!(entity_nodes(root, "characters/mara"), ["m"]);
        assert_eq!(wires(root, "key"), [("h".into(), EdgeRole::Reference, false), ("m".into(), EdgeRole::Reference, true)]);

        // Both tokens go: only the wire the mention made goes with them; the nodes stay.
        let done = run(root, json!([prompt("key", "the quay")]));
        assert_eq!(done.mentions.len(), 1);
        assert_eq!((done.mentions[0].did, done.mentions[0].token.as_str()), (MentionDid::Unwired, "@characters/mara"));
        assert_eq!(wires(root, "key"), [("h".into(), EdgeRole::Reference, false)]);
        assert_eq!(entity_nodes(root, "characters/mara"), ["m"]);
    }

    #[test]
    fn default_ports_by_what_the_source_outputs() {
        let s = studio("ports");
        let root = &s.0;
        std::fs::write(root.join("canvases/c/script.md"), "# Script").unwrap();
        run(
            root,
            json!([
                node("note", json!({"kind": "note", "text": "Dawn, cold"})),
                node("doc", json!({"kind": "asset", "path": "canvases/c/script.md"})),
                node("img", json!({"kind": "asset", "path": "canvases/c/assets/plate.png", "media": "image"})),
                node("key", json!({"kind": "gen", "makes": "image", "title": "Key"})),
                node("clip", json!({"kind": "gen", "makes": "video", "title": "Clip"})),
                node("line", json!({"kind": "gen", "makes": "speech", "title": "Line"})),
            ]),
        );
        let done = run(
            root,
            json!([card("vid", "video", "From @#key, @#img; mood @#note (@#doc). Extends @#clip, voice @#line. Self @#vid, frame-less @#nope.")]),
        );
        assert_eq!(
            wires(root, "vid"),
            [
                ("key".into(), EdgeRole::Reference, true),
                ("img".into(), EdgeRole::Reference, true),
                ("note".into(), EdgeRole::Text, true),
                ("doc".into(), EdgeRole::Text, true),
            ]
        );
        let not: Vec<(&str, &str)> =
            done.mentions.iter().filter(|m| m.did == MentionDid::NotWired).map(|m| (m.token.as_str(), m.why.as_deref().unwrap_or(""))).collect();
        assert_eq!(not.len(), 2, "{not:?}");
        assert_eq!(not[0].0, "@#clip");
        assert!(not[0].1.contains("outputs a video"), "{}", not[0].1);
        assert!(not[1].1.contains("outputs audio"), "{}", not[1].1);
        assert_eq!(done.mentions.len(), 6, "self and unknown ids are text");
    }

    #[test]
    fn port_limits_apply_and_the_edit_still_succeeds() {
        let s = studio("limits");
        let root = &s.0;
        let done = run(root, json!([card("say", "speech", "@characters/mara says hello")]));
        assert!(done.failed.is_empty());
        assert_eq!(done.mentions.len(), 1);
        assert_eq!(done.mentions[0].did, MentionDid::NotWired);
        assert!(done.mentions[0].why.as_deref().unwrap().contains("a speech card has no reference port"), "{:?}", done.mentions[0].why);
        assert!(entity_nodes(root, "characters/mara").is_empty(), "no node for a wire that can't be");
        assert_eq!(canvas(root).node("say").map(|n| matches!(&n.body, NodeBody::Gen { prompt, .. } if prompt.starts_with('@'))), Some(true));
        // A loop: a fed by b; b's prompt mentions a.
        run(
            root,
            json!([card("a", "image", "a"), card("b", "image", "b"), {"op": "add_edge", "edge": {"from": "b", "to": "a", "role": "reference"}}]),
        );
        let done = run(root, json!([prompt("b", "like @#a")]));
        assert_eq!(done.mentions[0].did, MentionDid::NotWired);
        assert!(done.mentions[0].why.as_deref().unwrap().contains("loop"));
    }

    #[test]
    fn a_wire_deleted_by_hand_stays_deleted_until_the_token_is_written_again() {
        let s = studio("hand");
        let root = &s.0;
        run(root, json!([card("key", "image", "@characters/mara on the quay")]));
        let id = canvas(root).edges[0].id.clone();
        run(root, json!([{"op": "remove_edge", "id": id}]));
        let done = run(root, json!([prompt("key", "@characters/mara on the quay at dawn")]));
        assert!(done.mentions.is_empty());
        assert!(wires(root, "key").is_empty());
        run(root, json!([prompt("key", "on the quay")]));
        run(root, json!([prompt("key", "@characters/mara on the quay")]));
        assert_eq!(wires(root, "key").len(), 1, "written again: wired again, from the node already there");
        assert_eq!(entity_nodes(root, "characters/mara").len(), 1);
        // The source deleted: the token stays as text, nothing is re-made.
        let m = entity_nodes(root, "characters/mara").remove(0);
        let done = run(root, json!([{"op": "remove_node", "id": m}, prompt("key", "@characters/mara on the quay, later")]));
        assert!(done.mentions.is_empty());
        assert!(wires(root, "key").is_empty());
    }

    #[test]
    fn tokens_outside_a_cards_prompt_do_nothing() {
        let s = studio("notes");
        let root = &s.0;
        let done = run(
            root,
            json!([
                node("n", json!({"kind": "note", "text": "@characters/mara"})),
                node("k", json!({"kind": "gen", "makes": "image", "title": "@characters/mara", "prompt": "@characters/nobody @mara"})),
            ]),
        );
        assert!(done.mentions.is_empty());
        assert!(canvas(root).edges.is_empty());
        assert!(entity_nodes(root, "characters/mara").is_empty());
    }

    fn asset_nodes(root: &Path, path: &str) -> Vec<String> {
        canvas(root).nodes.iter().filter(|n| matches!(&n.body, NodeBody::Asset { path: p, .. } if p == path)).map(|n| n.id.clone()).collect()
    }

    /// Mara with a named coat, a walk (a video) and a plain face.
    fn dressed(name: &str) -> Studio {
        let s = studio(name);
        let dir = s.0.join("world/characters/mara");
        std::fs::write(
            dir.join("mara.md"),
            "---\nname: Mara Venn\nrefs:\n  - file: refs/coat.png\n    name: Coat\n    defines: the oilskin coat\n  - refs/walk.mp4\n  - refs/face.png\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("refs")).unwrap();
        for f in ["coat.png", "walk.mp4", "face.png"] {
            std::fs::write(dir.join("refs").join(f), b"x").unwrap();
        }
        s
    }

    const COAT: &str = "world/characters/mara/refs/coat.png";

    #[test]
    fn a_reference_token_wires_its_file_beside_the_entity() {
        let s = dressed("refs");
        let root = &s.0;
        let rev = canvas(root).rev;
        let done = run(root, json!([card("key", "image", "@characters/mara#coat over @characters/mara, @characters/mara#nope")]));
        assert_eq!(done.rev, rev + 1, "one write");
        let tokens: Vec<(&str, MentionDid)> = done.mentions.iter().map(|m| (m.token.as_str(), m.did)).collect();
        assert_eq!(tokens, [("@characters/mara#coat", MentionDid::Wired), ("@characters/mara", MentionDid::Wired)], "an unknown reference is text");
        let coat = asset_nodes(root, COAT);
        assert_eq!(coat.len(), 1);
        assert_eq!(done.mentions[0].node.as_ref(), Some(&coat[0]));
        let mara = entity_nodes(root, "characters/mara");
        assert_eq!(wires(root, "key"), [(coat[0].clone(), EdgeRole::Reference, true), (mara[0].clone(), EdgeRole::Reference, true)]);
        let c = canvas(root);
        let (asset, key) = (c.node(&coat[0]).unwrap(), c.node("key").unwrap());
        assert!(asset.x + asset.w < key.x && asset.y == key.y, "left of the card: {asset:?}");
        assert!(matches!(&asset.body, NodeBody::Asset { media: silo_proto::studio::StudioMedia::Image, .. }));

        // Another card reuses the Asset node; a video reference gets no wire and no node.
        let done = run(root, json!([card("two", "image", "@characters/mara#coat again"), card("vid", "image", "@characters/mara#walk")]));
        assert_eq!(done.mentions[0].node, None, "reused");
        assert_eq!(wires(root, "two"), [(coat[0].clone(), EdgeRole::Reference, true)]);
        assert_eq!(done.mentions[1].did, MentionDid::NotWired);
        assert!(done.mentions[1].why.as_deref().unwrap().contains("Mara Venn, walk outputs a video"), "{:?}", done.mentions[1].why);
        assert!(asset_nodes(root, "world/characters/mara/refs/walk.mp4").is_empty());

        // The reference's token goes: its wire goes, the entity's stays, the node stays.
        let done = run(root, json!([prompt("key", "@characters/mara alone")]));
        assert_eq!((done.mentions.len(), done.mentions[0].token.as_str(), done.mentions[0].did), (1, "@characters/mara#coat", MentionDid::Unwired));
        assert_eq!(wires(root, "key"), [(mara[0].clone(), EdgeRole::Reference, true)]);
        assert_eq!(asset_nodes(root, COAT), coat);
        // A new reference token adds its node; with two Asset nodes of the file, the first is wired.
        let done = run(root, json!([prompt("key", "@characters/mara in @characters/mara#face")]));
        let face = asset_nodes(root, "world/characters/mara/refs/face.png");
        assert_eq!((face.len(), done.mentions[0].node.as_ref()), (1, Some(&face[0])));
        let done = run(
            root,
            json!([
                node("face2", json!({"kind": "asset", "path": "world/characters/mara/refs/face.png"})),
                card("three", "image", "@characters/mara#face")
            ]),
        );
        assert_eq!(done.mentions[0].node, None);
        assert_eq!(wires(root, "three"), [(face[0].clone(), EdgeRole::Reference, true)], "the first in canvas order");
    }

    #[test]
    fn undoing_a_reference_mention_is_one_edit_back() {
        let s = dressed("refs-undo");
        let root = &s.0;
        run(root, json!([card("key", "image", "the quay")]));
        let before = canvas(root);
        let done = run(root, json!([prompt("key", "@characters/mara#coat and @characters/mara on the quay")]));
        assert_eq!(done.rev, before.rev + 1);
        let added: Vec<String> = done.mentions.iter().filter_map(|m| m.node.clone()).collect();
        assert_eq!(added.len(), 2);
        let mut ops = vec![prompt("key", "the quay")];
        ops.extend(added.iter().map(|id| json!({"op": "remove_node", "id": id})));
        let back = run(root, Value::Array(ops));
        assert_eq!(back.rev, before.rev + 2);
        let after = canvas(root);
        assert_eq!((after.nodes, after.edges), (before.nodes, before.edges));
    }

    #[test]
    fn undo_is_one_edit_back() {
        let s = studio("undo");
        let root = &s.0;
        run(root, json!([card("key", "image", "the quay")]));
        let before = canvas(root);
        let done = run(root, json!([prompt("key", "@characters/mara on the quay")]));
        assert_eq!(done.rev, before.rev + 1);
        let added = done.mentions[0].node.clone().unwrap();
        // The inverse the app sends: the prompt back, and the node the mention added.
        let back = run(root, json!([prompt("key", "the quay"), {"op": "remove_node", "id": added}]));
        assert_eq!(back.rev, before.rev + 2);
        assert!(back.mentions.is_empty(), "the node took its wire with it: {:?}", back.mentions);
        let after = canvas(root);
        assert_eq!((after.nodes, after.edges), (before.nodes, before.edges));
    }
}
