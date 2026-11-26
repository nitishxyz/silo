//! `studio.production` (docs/STUDIO.md §4.6): where the project stands,
//! derived from the files on every call (nothing is stored): each entity's
//! sheet, approval, voice and video sheet; each canvas's stages; the model
//! picks; and the next steps, a pure function of those ([`next_steps`]).
//!
//! Cost: `studio.toml`, the world (a folder listing per section and entity,
//! each entity file read, each sheet stat'ed), then per canvas its
//! `canvas.json` parsed once, a stat per wired file of a card with takes
//! (out of date), and two small folder reads (`script.md`, `out/`). A dozen
//! canvases of a hundred nodes is a few milliseconds.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use serde_json::{Map, Value};
use silo_proto::{
    control::RpcError,
    studio::{
        Approval, AssetState, Canvas, Cut, EpisodeState, FrameRole, GenKind, GenStatus, NodeBody, Production, SectionKind, Step, StepKind,
        StudioMedia,
    },
};

use super::{approval, canvas, project, world};
use crate::files::paths;

/// Sections whose entities need a sheet (the next steps' rule).
const SHEETED: [&str; 3] = ["characters", "places", "props"];

/// What [`next_steps`] reads: the derived state, and what it doesn't show.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// `world/overview.md` says something beyond the template.
    pub overview: bool,
    pub assets: Vec<AssetState>,
    pub episodes: Vec<Episode>,
}

/// A canvas's state and how many of its shot frames hold a picture (an
/// image or a video): a shot made straight to video needs no keyframe.
#[derive(Clone, Debug)]
pub struct Episode {
    pub state: EpisodeState,
    pub pictured: u32,
}

/// `studio.production`.
///
/// # Errors
/// `not_found` without `studio.toml`; `bad_request` when it doesn't parse.
pub fn production(root: &Path) -> Result<Production, RpcError> {
    let doc = project::read_doc(root)?.ok_or_else(|| RpcError::not_found("studio.toml (the block shows Setup)"))?;
    let canonical = paths::canonical_root(root)?;
    let world = world::sections_with_fields(&canonical);
    let sections: Vec<_> = world.iter().map(|(s, _)| s.clone()).collect();

    let mut loaded: Vec<(silo_proto::studio::CanvasInfo, Canvas)> = Vec::new();
    for name in canvas::names(&canonical) {
        let Ok(doc) = canvas::load(&canonical, &name) else { continue };
        let info = canvas::info_of(&canonical, &doc);
        let mut c = doc.canvas;
        canvas::mark_out_of_date(&canonical, &mut c, Some(&sections));
        loaded.push((info, c));
    }
    let order: Vec<String> =
        project::ordered(loaded.iter().map(|(i, _)| i.clone()).collect(), &project::order_of(&doc)).into_iter().map(|i| i.name).collect();
    let mut by_name: HashMap<String, Canvas> = loaded.into_iter().map(|(i, c)| (i.name, c)).collect();
    let canvases: Vec<Canvas> = order.iter().filter_map(|n| by_name.remove(n)).collect();

    let ids: Vec<&str> = sections.iter().flat_map(|s| &s.entities).map(|e| e.id.as_str()).collect();
    let mut used: HashMap<&str, u32> = HashMap::new();
    for c in &canvases {
        for id in uses(c, &ids) {
            *used.entry(id).or_default() += 1;
        }
    }

    let mut assets = Vec::new();
    for (section, fields) in &world {
        if section.kind != SectionKind::Entities {
            continue;
        }
        for (info, fields) in section.entities.iter().zip(fields) {
            assets.push(asset(&canonical, &section.id, info, fields, used.get(info.id.as_str()).copied().unwrap_or(0)));
        }
    }
    let episodes: Vec<Episode> = canvases.iter().map(|c| episode(&canonical, c)).collect();
    let overview = std::fs::read_to_string(canonical.join("world/overview.md")).is_ok_and(|t| overview_written(&t));
    let facts = Facts { overview, assets, episodes };
    let next = next_steps(&facts);
    Ok(Production { assets: facts.assets, episodes: facts.episodes.into_iter().map(|e| e.state).collect(), models: project::models_of(&doc), next })
}

fn asset(root: &Path, section: &str, info: &silo_proto::studio::EntityInfo, fields: &Map<String, Value>, used: u32) -> AssetState {
    let voice = (section == "characters" && !approval::speechless(fields.get("voice"))).then_some(info.voice);
    let video_sheet = matches!(section, "characters" | "props").then(|| world::has_video_ref(root, info));
    AssetState {
        id: info.id.clone(),
        name: info.name.clone(),
        section: section.to_owned(),
        sheet: info.sheet.clone(),
        approval: info.approval,
        voice,
        video_sheet,
        used,
    }
}

/// The entities among `ids` that canvas `c` uses, by `studio.entity`'s
/// `used_in` rules: an Entity node of it, an Asset node of a file in its
/// folder, a card whose last request names one of its files.
fn uses<'a>(c: &Canvas, ids: &[&'a str]) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    let requests: Vec<String> = c
        .nodes
        .iter()
        .filter_map(|n| match &n.body {
            NodeBody::Gen { input: Some(input), .. } => Some(input.to_string()),
            _ => None,
        })
        .collect();
    for &id in ids {
        let dir = format!("world/{id}/");
        let hit = c.nodes.iter().any(|n| match &n.body {
            NodeBody::Entity { id: e } => e == id,
            NodeBody::Asset { path, .. } => path.starts_with(&dir),
            _ => false,
        }) || requests.iter().any(|r| r.contains(&dir));
        if hit {
            out.insert(id);
        }
    }
    out
}

/// What a node is as a picture: an image or a video (a card by its picked
/// take, an asset by its path).
fn picture(body: &NodeBody) -> Option<StudioMedia> {
    match body {
        NodeBody::Asset { path, media, .. } => {
            let media = if *media == StudioMedia::Unknown { StudioMedia::of_path(path) } else { *media };
            matches!(media, StudioMedia::Image | StudioMedia::Video).then_some(media)
        }
        b @ NodeBody::Gen { makes: GenKind::Image | GenKind::Video, .. } => {
            b.take().map(|t| t.media).filter(|m| matches!(m, StudioMedia::Image | StudioMedia::Video))
        }
        _ => None,
    }
}

/// Canvas `c`'s stages (canonical `root`).
fn episode(root: &Path, c: &Canvas) -> Episode {
    let shot_ids: Vec<&str> =
        c.nodes.iter().filter(|n| matches!(n.body, NodeBody::Frame { role: FrameRole::Shot, .. })).map(|n| n.id.as_str()).collect();
    let parents: HashMap<&str, Option<&str>> = c.nodes.iter().map(|n| (n.id.as_str(), n.parent.as_deref())).collect();
    // The shot frame a node is in, at any depth (bounded: a cycle of parents ends it).
    let shot_of = |id: &str| {
        let mut at = parents.get(id).copied().flatten();
        for _ in 0..c.nodes.len() {
            let here = at?;
            if shot_ids.contains(&here) {
                return Some(here);
            }
            at = parents.get(here).copied().flatten();
        }
        None
    };
    let (mut images, mut videos): (BTreeSet<&str>, BTreeSet<&str>) = Default::default();
    let (mut sound, mut failed, mut running, mut newest_take) = (0u32, 0u32, 0u32, 0u64);
    for n in &c.nodes {
        if let Some(media) = picture(&n.body)
            && let Some(shot) = shot_of(&n.id)
        {
            if media == StudioMedia::Image {
                images.insert(shot)
            } else {
                videos.insert(shot)
            };
        }
        match &n.body {
            body @ NodeBody::Gen { makes, status, .. } => {
                let take = body.take();
                if matches!(makes, GenKind::Speech | GenKind::Sound | GenKind::Music) && take.is_some() {
                    sound += 1;
                }
                if *makes != GenKind::Image
                    && let Some(t) = take
                {
                    newest_take = newest_take.max(t.at);
                }
                match status {
                    GenStatus::Failed => failed += 1,
                    GenStatus::Queued | GenStatus::Running => running += 1,
                    _ => {}
                }
            }
            NodeBody::Asset { path, media, .. }
                if *media == StudioMedia::Audio || (*media == StudioMedia::Unknown && StudioMedia::of_path(path) == StudioMedia::Audio) =>
            {
                sound += 1;
            }
            _ => {}
        }
    }
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let folder = root.join("canvases").join(&c.name);
    let script = std::fs::read_to_string(folder.join("script.md")).is_ok_and(|t| !t.trim().is_empty());
    let cut = cut_of(&folder, &c.name).map(|mut cut| {
        cut.stale = newest_take > cut.at;
        cut
    });
    let state = EpisodeState {
        canvas: c.name.clone(),
        title: c.title.clone(),
        episode: c.episode,
        script,
        shots: count(shot_ids.len()),
        keyframes: count(images.len()),
        videos: count(videos.len()),
        sound,
        out_of_date: count(c.out_of_date.len()),
        failed,
        running,
        cut,
    };
    Episode { state, pictured: count(images.union(&videos).count()) }
}

/// The newest `out/<name>-v<N>.mp4` in canvas folder `folder`, with the
/// older versions.
fn cut_of(folder: &Path, name: &str) -> Option<Cut> {
    let entries = std::fs::read_dir(folder.join("out")).ok()?;
    let mut cuts: Vec<(u32, String, u64)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file = e.file_name().to_str()?.to_owned();
            let version = file.strip_prefix(name)?.strip_prefix("-v")?.strip_suffix(".mp4")?;
            let version: u32 = (!version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())).then(|| version.parse().ok())??;
            let meta = e.path().metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((version, format!("canvases/{name}/out/{file}"), super::disk::mtime_ms(&meta)))
        })
        .collect();
    cuts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut cuts = cuts.into_iter();
    let (version, path, at) = cuts.next()?;
    Some(Cut { path, version, older: cuts.map(|(_, p, _)| p).collect(), at, stale: false })
}

/// Whether `overview.md` says anything beyond headings and the template
/// `studio.init` writes.
pub fn overview_written(text: &str) -> bool {
    let template: Vec<&str> = world::OVERVIEW.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    text.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with('#') && !template.contains(&l))
}

fn plural(n: u32, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// A canvas in a button: `episode 1`, else its title.
fn short(e: &EpisodeState) -> String {
    e.episode.map_or_else(|| e.title.clone(), |n| format!("episode {n}"))
}

/// A canvas in a message: `episode 1 ("Pilot", canvas ep01-pilot)`.
fn named(e: &EpisodeState) -> String {
    match e.episode {
        Some(n) => format!("episode {n} (\"{}\", canvas {})", e.title, e.canvas),
        None => format!("\"{}\" (canvas {})", e.title, e.canvas),
    }
}

fn step(kind: StepKind, text: String, ask: Option<String>, entity: Option<&str>, canvas: Option<&str>) -> Step {
    Step { kind, text, ask, entity: entity.map(str::to_owned), canvas: canvas.map(str::to_owned) }
}

/// The step an episode is at, by production order; `None`: nothing left
/// but out-of-date cards. A canvas with shots needs no script to go on.
fn stage(e: &Episode) -> Option<Step> {
    let s = &e.state;
    let (here, it) = (short(s), named(s));
    let canvas = Some(s.canvas.as_str());
    let make = |kind, text: String, ask: String| Some(step(kind, text, Some(ask), None, canvas));
    if !s.script && s.shots == 0 {
        return make(StepKind::Script, format!("Write {here}'s script"), format!("Write the script of {it} in canvases/{}/script.md.", s.canvas));
    }
    if s.shots == 0 {
        return make(
            StepKind::Shots,
            format!("Break {here} into shots"),
            format!("Break the script of {it} into scene and shot frames on its canvas, with a note on each shot."),
        );
    }
    if e.pictured < s.shots {
        let n = s.shots - e.pictured;
        let text = if n == 1 { format!("Make the keyframe {here} still needs") } else { format!("Make the {n} keyframes {here} still needs") };
        return make(
            StepKind::Keyframes,
            text,
            format!(
                "Make the missing keyframes of {it}: {} of its {} have none. Wire each shot's characters, place and props from their sheets.",
                n,
                plural(s.shots, "shot", "shots")
            ),
        );
    }
    if s.videos < s.shots {
        let n = s.shots - s.videos;
        return make(
            StepKind::Video,
            format!("Make the video for {} of {here}", plural(n, "shot", "shots")),
            format!("Make the missing video of {it}: {n} of its {} have none. Start each from its keyframe.", plural(s.shots, "shot", "shots")),
        );
    }
    match &s.cut {
        None => make(
            StepKind::Cut,
            format!("Cut {here}"),
            format!("Assemble the cut of {it} from the picked takes in shot order, with its sound, as canvases/{0}/out/{0}-v1.mp4.", s.canvas),
        ),
        Some(cut) if cut.stale => make(
            StepKind::Cut,
            format!("Cut {here} again"),
            format!(
                "Cut {it} again: takes were made after {}. Write it as canvases/{1}/out/{1}-v{2}.mp4 and keep the older cuts.",
                cut.path,
                s.canvas,
                cut.version + 1
            ),
        ),
        Some(_) => None,
    }
}

/// The next steps (docs/STUDIO.md §5), the first three that apply, in
/// this order: an empty overview (onboarding); a character, place or prop
/// used on a canvas (with no canvas yet, any) without a sheet; such an
/// entity's sheet not approved or changed since (the user's: `entity`, no
/// `ask`); a speaking character without a voice; no episode canvas; then
/// each episode at its stage (script, shots, keyframes, video, cut), all
/// episodes' earlier stages before any later one, by episode; then cards
/// out of date on any canvas.
#[must_use]
pub fn next_steps(facts: &Facts) -> Vec<Step> {
    const MAX: usize = 3;
    let mut out: Vec<Step> = Vec::new();
    let no_canvas = facts.episodes.is_empty();
    let in_play = |a: &&AssetState| no_canvas || a.used > 0;
    if !facts.overview {
        out.push(step(
            StepKind::Onboarding,
            "Tell the agent about the show".into(),
            Some("Interview me to start this studio: premise, genre, tone, audience, references and format, then write world/overview.md and world/style.md.".into()),
            None,
            None,
        ));
    }
    let it = |a: &AssetState| format!("{} (world entity {})", a.name, a.id);
    for a in facts.assets.iter().filter(in_play).filter(|a| SHEETED.contains(&a.section.as_str()) && a.sheet.is_none()) {
        let ask = match a.section.as_str() {
            "characters" => format!("Generate a character sheet for {}: three views in one image, and set it as its sheet.", it(a)),
            "props" => format!("Generate a prop sheet for {}: two views in one image, and set it as its sheet.", it(a)),
            _ => format!("Generate the establishing image for {} and set it as its sheet.", it(a)),
        };
        out.push(step(StepKind::Sheet, format!("Make {}'s sheet", a.name), Some(ask), Some(&a.id), None));
    }
    for a in facts.assets.iter().filter(in_play).filter(|a| a.sheet.is_some() && a.approval != Approval::Approved) {
        let text = if a.approval == Approval::Changed {
            format!("Review {}'s changed sheet and approve it again", a.name)
        } else {
            format!("Review and approve {}'s sheet", a.name)
        };
        out.push(step(StepKind::Approve, text, None, Some(&a.id), None));
    }
    for a in facts.assets.iter().filter(in_play).filter(|a| a.voice == Some(false)) {
        let ask = format!("Cast a voice for {}: offer two or three with a sample line, then set the one I pick.", it(a));
        out.push(step(StepKind::Voice, format!("Cast {}'s voice", a.name), Some(ask), Some(&a.id), None));
    }
    let episodes: Vec<&Episode> = facts.episodes.iter().filter(|e| e.state.episode.is_some()).collect();
    if episodes.is_empty() {
        out.push(step(
            StepKind::Episode,
            "Plan episode 1".into(),
            Some("Plan episode 1 from world/overview.md: make its canvas and write its script.".into()),
            None,
            None,
        ));
    }
    let mut staged: Vec<(usize, usize, Step)> = Vec::new();
    for (i, e) in episodes.iter().enumerate() {
        if let Some(s) = stage(e) {
            let rank = [StepKind::Script, StepKind::Shots, StepKind::Keyframes, StepKind::Video, StepKind::Cut].iter().position(|k| *k == s.kind);
            staged.push((rank.unwrap_or(usize::MAX), i, s));
        }
    }
    staged.sort_by_key(|(rank, i, _)| (*rank, *i));
    out.extend(staged.into_iter().map(|(_, _, s)| s));
    for e in facts.episodes.iter().map(|e| &e.state).filter(|e| e.out_of_date > 0) {
        let n = e.out_of_date;
        let cards = plural(n, "card", "cards");
        out.push(step(
            StepKind::OutOfDate,
            format!("Update {} on {}", plural(n, "out-of-date card", "out-of-date cards"), short(e)),
            Some(format!(
                "{cards} on {} {} out of date: {} inputs changed since {} take. Tell me which and what changed, then rerun the ones I agree to.",
                named(e),
                if n == 1 { "is" } else { "are" },
                if n == 1 { "its" } else { "their" },
                if n == 1 { "its" } else { "their" },
            )),
            None,
            Some(&e.canvas),
        ));
    }
    out.truncate(MAX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(id: &str, sheet: bool, approval: Approval, voice: Option<bool>, used: u32) -> AssetState {
        let (section, _) = id.split_once('/').unwrap();
        let name = id.rsplit('/').next().unwrap();
        let name = name[..1].to_uppercase() + &name[1..];
        AssetState {
            id: id.into(),
            name,
            section: section.into(),
            sheet: sheet.then(|| format!("world/{id}/sheet.png")),
            approval,
            voice,
            video_sheet: None,
            used,
        }
    }

    #[allow(clippy::too_many_arguments, reason = "a table row")]
    fn ep(n: Option<u32>, name: &str, script: bool, shots: u32, pictured: u32, keyframes: u32, videos: u32, cut: Option<(u32, bool)>) -> Episode {
        let state = EpisodeState {
            canvas: name.into(),
            title: name.into(),
            episode: n,
            script,
            shots,
            keyframes,
            videos,
            sound: 0,
            out_of_date: 0,
            failed: 0,
            running: 0,
            cut: cut.map(|(version, stale)| Cut {
                path: format!("canvases/{name}/out/{name}-v{version}.mp4"),
                version,
                older: Vec::new(),
                at: 0,
                stale,
            }),
        };
        Episode { state, pictured }
    }

    fn kinds(steps: &[Step]) -> Vec<(StepKind, String)> {
        steps.iter().map(|s| (s.kind, s.entity.clone().or_else(|| s.canvas.clone()).unwrap_or_default())).collect()
    }

    fn done(n: u32, name: &str) -> Episode {
        ep(Some(n), name, true, 4, 4, 4, 4, Some((1, false)))
    }

    /// What, the facts, the steps wanted (kind, entity or canvas).
    type Row = (&'static str, Facts, Vec<(StepKind, &'static str)>);

    #[test]
    fn next_steps_follow_production_order() {
        use Approval::{Approved as A, Changed as C, None as N};
        use StepKind::*;
        let mara = |sheet, approval, voice, used| asset("characters/mara", sheet, approval, voice, used);
        let quay = |sheet, approval, used| asset("places/quay", sheet, approval, None, used);
        let unused_prop = asset("props/kite", false, N, None, 0);
        let faction = asset("factions/guild", false, N, None, 3);
        let ready = vec![mara(true, A, Some(true), 1), quay(true, A, 1)];
        #[rustfmt::skip]
        let table: Vec<Row> = vec![
            ("an empty studio: onboarding, then the first episode",
             Facts { overview: false, assets: vec![], episodes: vec![] },
             vec![(Onboarding, ""), (Episode, "")]),
            ("no canvas yet: every entity counts",
             Facts { overview: true, assets: vec![mara(false, N, Some(false), 0), quay(false, N, 0), unused_prop.clone()], episodes: vec![] },
             vec![(Sheet, "characters/mara"), (Sheet, "places/quay"), (Sheet, "props/kite")]),
            ("with canvases only used entities count; custom sections need no sheet",
             Facts { overview: true, assets: vec![mara(false, N, Some(false), 1), unused_prop.clone(), faction.clone()], episodes: vec![done(1, "ep01")] },
             vec![(Sheet, "characters/mara"), (Voice, "characters/mara")]),
            ("approval is the user's, after sheets; changed counts as not approved",
             Facts { overview: true, assets: vec![mara(true, C, Some(true), 1), quay(false, N, 1), asset("props/oar", true, N, None, 1)], episodes: vec![done(1, "ep01")] },
             vec![(Sheet, "places/quay"), (Approve, "characters/mara"), (Approve, "props/oar")]),
            ("a speechless character (voice None) needs no voice",
             Facts { overview: true, assets: vec![mara(true, A, None, 1)], episodes: vec![done(1, "ep01")] },
             vec![]),
            ("no episode canvas: a moodboard doesn't count",
             Facts { overview: true, assets: ready.clone(), episodes: vec![ep(None, "moodboard", false, 0, 0, 0, 0, None)] },
             vec![(Episode, "")]),
            ("episodes by stage: earlier stages of any episode first, then by number",
             Facts { overview: true, assets: ready.clone(), episodes: vec![
                 ep(Some(1), "ep01", true, 5, 5, 5, 2, None),
                 ep(Some(2), "ep02", true, 0, 0, 0, 0, None),
                 ep(Some(3), "ep03", false, 0, 0, 0, 0, None),
                 ep(Some(4), "ep04", true, 6, 2, 2, 0, None),
             ] },
             vec![(Script, "ep03"), (Shots, "ep02"), (Keyframes, "ep04")]),
            ("a shot made straight to video needs no keyframe; video then cut",
             Facts { overview: true, assets: ready.clone(), episodes: vec![
                 ep(Some(1), "ep01", true, 3, 3, 1, 3, None),
                 ep(Some(2), "ep02", true, 3, 3, 3, 1, None),
             ] },
             vec![(Video, "ep02"), (Cut, "ep01")]),
            ("a canvas with shots and no script goes on",
             Facts { overview: true, assets: ready.clone(), episodes: vec![ep(Some(1), "ep01", false, 2, 0, 0, 0, None)] },
             vec![(Keyframes, "ep01")]),
            ("a stale cut is cut again; a fresh one is done; out of date last, on any canvas",
             Facts { overview: true, assets: ready.clone(), episodes: vec![
                 { let mut e = done(1, "ep01"); e.state.out_of_date = 2; e },
                 ep(Some(2), "ep02", true, 4, 4, 4, 4, Some((3, true))),
                 { let mut e = ep(None, "moodboard", false, 0, 0, 0, 0, None); e.state.out_of_date = 1; e },
             ] },
             vec![(Cut, "ep02"), (OutOfDate, "ep01"), (OutOfDate, "moodboard")]),
            ("at most three",
             Facts { overview: false, assets: vec![mara(false, N, Some(false), 0), quay(false, N, 0)], episodes: vec![] },
             vec![(Onboarding, ""), (Sheet, "characters/mara"), (Sheet, "places/quay")]),
        ];
        for (what, facts, want) in table {
            let want: Vec<(StepKind, String)> = want.into_iter().map(|(k, t)| (k, t.to_owned())).collect();
            assert_eq!(kinds(&next_steps(&facts)), want, "{what}");
        }
    }

    #[test]
    fn steps_read_as_whole_messages_naming_ids() {
        let facts = Facts {
            overview: true,
            assets: vec![
                AssetState { name: "Mara Venn".into(), ..asset("characters/mara", false, Approval::None, Some(false), 1) },
                AssetState { name: "Old Quay".into(), ..asset("places/quay", true, Approval::Changed, None, 1) },
            ],
            episodes: vec![Episode {
                state: EpisodeState { title: "Pilot".into(), ..ep(Some(1), "ep01-pilot", true, 0, 0, 0, 0, None).state },
                pictured: 0,
            }],
        };
        let steps = next_steps(&facts);
        assert_eq!(steps[0].text, "Make Mara Venn's sheet");
        assert_eq!(
            steps[0].ask.as_deref(),
            Some("Generate a character sheet for Mara Venn (world entity characters/mara): three views in one image, and set it as its sheet.")
        );
        assert_eq!(
            (steps[1].text.as_str(), steps[1].ask.as_deref(), steps[1].entity.as_deref()),
            ("Review Old Quay's changed sheet and approve it again", None, Some("places/quay"))
        );
        assert_eq!(steps[2].text, "Cast Mara Venn's voice");

        let shots =
            |state: EpisodeState, pictured| next_steps(&Facts { overview: true, assets: vec![], episodes: vec![Episode { state, pictured }] });
        let base = EpisodeState { title: "Pilot".into(), ..ep(Some(1), "ep01-pilot", true, 0, 0, 0, 0, None).state };
        let s = &shots(base.clone(), 0)[0];
        assert_eq!(s.text, "Break episode 1 into shots");
        assert_eq!(
            s.ask.as_deref(),
            Some("Break the script of episode 1 (\"Pilot\", canvas ep01-pilot) into scene and shot frames on its canvas, with a note on each shot.")
        );
        let s = &shots(EpisodeState { shots: 8, keyframes: 3, ..base.clone() }, 3)[0];
        assert_eq!(s.text, "Make the 5 keyframes episode 1 still needs");
        assert_eq!(
            s.ask.as_deref(),
            Some(
                "Make the missing keyframes of episode 1 (\"Pilot\", canvas ep01-pilot): 5 of its 8 shots have none. Wire each shot's characters, place and props from their sheets."
            )
        );
        let s = &shots(
            EpisodeState {
                shots: 2,
                keyframes: 2,
                videos: 2,
                cut: ep(None, "ep01-pilot", true, 0, 0, 0, 0, Some((2, true))).state.cut,
                ..base.clone()
            },
            2,
        )[0];
        assert_eq!(s.text, "Cut episode 1 again");
        assert!(s.ask.as_deref().unwrap().contains("as canvases/ep01-pilot/out/ep01-pilot-v3.mp4"), "{s:?}");
        let s = &shots(EpisodeState { shots: 1, keyframes: 1, videos: 1, cut: None, out_of_date: 0, ..base.clone() }, 1)[0];
        assert_eq!(
            s.ask.as_deref(),
            Some(
                "Assemble the cut of episode 1 (\"Pilot\", canvas ep01-pilot) from the picked takes in shot order, with its sound, as canvases/ep01-pilot/out/ep01-pilot-v1.mp4."
            )
        );
        let s = &shots(
            EpisodeState {
                shots: 1,
                keyframes: 1,
                videos: 1,
                out_of_date: 1,
                cut: ep(None, "x", true, 0, 0, 0, 0, Some((1, false))).state.cut,
                ..base
            },
            1,
        )[0];
        assert_eq!(s.text, "Update 1 out-of-date card on episode 1");
        assert_eq!(
            s.ask.as_deref(),
            Some(
                "1 card on episode 1 (\"Pilot\", canvas ep01-pilot) is out of date: its inputs changed since its take. Tell me which and what changed, then rerun the ones I agree to."
            )
        );
    }

    #[test]
    fn an_overview_is_written_beyond_its_template() {
        assert!(!overview_written(""));
        assert!(!overview_written(world::OVERVIEW));
        assert!(!overview_written("# Overview\n\n## Premise\n\n"));
        assert!(overview_written(&format!("{}\nA salvage diver finds a bell.\n", world::OVERVIEW)));
    }
}
