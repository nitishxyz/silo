//! Live catalogs (docs/MEDIA.md §3.2): an aggregator's endpoints with their input schemas, grouped into one model
//! per family (`bytedance/seedance-2.5/*`: text-to-video, image-to-video, reference-to-video, video-edit, …).
use otto_proto::MediaKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One endpoint: what `POST <id>` takes (`schema`) and makes (`kind`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub about: String,
    pub kind: MediaKind,
    /// The provider's family (fal's `group.key`); else read from the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `text2video`, `image2video`, `video2video`, …
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    /// Its input JSON Schema, trimmed; `None` until fetched (fal's come one endpoint at a time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// One model: its endpoints by mode.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub key: String,
    pub name: String,
    pub about: String,
    pub kind: MediaKind,
    /// `(mode, endpoint)` in catalog order; a lone endpoint without a mode word has mode `""`.
    pub modes: Vec<(String, Endpoint)>,
}

/// A provider's cached catalog (`<state>/media-catalog/<provider>.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Live {
    /// Unix seconds.
    pub fetched_at: u64,
    pub endpoints: Vec<Endpoint>,
}

/// Id segments naming a mode (the last one in an id counts).
const MODES: &[&str] = &[
    "text-to-video",
    "image-to-video",
    "reference-to-video",
    "video-to-video",
    "video-edit",
    "video-extend",
    "text-to-image",
    "image-to-image",
    "edit",
    "motion-transfer",
    "object-swap",
    "restyle",
    "first-last-frame",
    "image-reference",
    "video-reference",
    "motion-control",
    "standard",
    "text-to-speech",
    "text-to-audio",
    "text-to-music",
];

fn mode_at(segments: &[&str]) -> Option<usize> {
    segments.iter().rposition(|s| MODES.contains(s))
}

/// The family an endpoint belongs to: the provider's group, else its id without the mode segment.
fn key(e: &Endpoint) -> String {
    if let Some(group) = e.group.as_ref().filter(|g| !g.is_empty()) {
        return group.clone();
    }
    let segments: Vec<&str> = e.id.split('/').collect();
    match mode_at(&segments) {
        Some(at) if segments.len() > 1 => segments.iter().enumerate().filter(|(i, _)| *i != at).map(|(_, s)| *s).collect::<Vec<_>>().join("/"),
        _ => e.id.clone(),
    }
}

/// `endpoints` grouped into families, in order; each mode named by what its id has that its siblings' don't.
#[must_use]
pub fn families(endpoints: &[Endpoint]) -> Vec<Family> {
    let mut out: Vec<Family> = Vec::new();
    for e in endpoints {
        let key = key(e);
        match out.iter_mut().find(|f| f.key == key && f.kind == e.kind) {
            Some(f) => f.modes.push((String::new(), e.clone())),
            None => out.push(Family { key, name: String::new(), about: String::new(), kind: e.kind, modes: vec![(String::new(), e.clone())] }),
        }
    }
    for f in &mut out {
        let ids: Vec<Vec<&str>> = f.modes.iter().map(|(_, e)| e.id.split('/').collect()).collect();
        let names: Vec<String> = if let [one] = ids.as_slice() {
            vec![mode_at(one).map_or_else(String::new, |at| one[at].to_owned())]
        } else {
            let shortest = ids.iter().map(Vec::len).min().unwrap_or(0);
            let prefix = (0..shortest).take_while(|&i| ids.iter().all(|s| s[i] == ids[0][i])).count();
            let suffix = (0..shortest - prefix).take_while(|&i| ids.iter().all(|s| s[s.len() - 1 - i] == ids[0][ids[0].len() - 1 - i])).count();
            ids.iter().map(|s| s[prefix..s.len() - suffix].join("/")).collect()
        };
        let unique =
            names.iter().all(|n| !n.is_empty() || names.len() == 1) && names.iter().collect::<std::collections::HashSet<_>>().len() == names.len();
        for ((mode, e), name) in f.modes.iter_mut().zip(names) {
            *mode = if unique { name } else { e.id.clone() };
        }
        f.name = family_name(&f.modes.iter().map(|(_, e)| e.name.as_str()).collect::<Vec<_>>());
        f.about = f.modes.iter().map(|(_, e)| e.about.as_str()).find(|a| !a.is_empty()).unwrap_or("").to_owned();
    }
    // The same name twice: each says where it differs (`Kling 3.0 (pro)`, `Kling 3.0 (std)`).
    let lower: Vec<String> = out.iter().map(|f| f.name.to_lowercase()).collect();
    let tails: Vec<Option<String>> = out
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let same: Vec<Vec<&str>> =
                out.iter().enumerate().filter(|(j, _)| lower[*j] == lower[i]).map(|(_, g)| g.key.split('/').collect()).collect();
            if same.len() < 2 {
                return None;
            }
            let prefix = (0..).take_while(|&k| same.iter().all(|s| s.len() > k && s[k] == same[0][k])).count();
            let tail = f.key.split('/').skip(prefix).collect::<Vec<_>>().join("/");
            (!tail.is_empty()).then_some(tail)
        })
        .collect();
    for (f, tail) in out.iter_mut().zip(tails) {
        if let Some(tail) = tail {
            f.name = format!("{} ({tail})", f.name);
        }
    }
    out
}

/// One name for a family's endpoints: theirs when they agree; their common first words (fal's `H3 Max Image to
/// Video`, `H3 Max Reference to Video` → `H3 Max`); else the longest (`Kling 3.0 4k` over `Kling 3.0`).
fn family_name(names: &[&str]) -> String {
    let longest = names.iter().max_by_key(|n| n.len()).copied().unwrap_or("");
    if names.iter().all(|n| *n == longest) {
        return longest.to_owned();
    }
    let words: Vec<Vec<&str>> = names.iter().map(|n| n.split_whitespace().collect()).collect();
    let common = (0..).take_while(|&k| words.iter().all(|w| w.len() > k && w[k] == words[0][k])).count();
    let prefix = words[0][..common].join(" ");
    let prefix = prefix.trim_end_matches(|c: char| !c.is_alphanumeric());
    if prefix.is_empty() || names.iter().any(|n| n.trim() == prefix) { longest.to_owned() } else { prefix.to_owned() }
}

/// `<state>/media-catalog/<provider>.json`.
pub(crate) fn path(state: &std::path::Path, provider: &str) -> std::path::PathBuf {
    state.join("media-catalog").join(format!("{provider}.json"))
}

pub(crate) fn load(state: &std::path::Path, provider: &str) -> Option<Live> {
    serde_json::from_slice(&std::fs::read(path(state, provider)).ok()?).ok()
}

pub(crate) fn save(state: &std::path::Path, provider: &str, live: &Live) -> std::io::Result<()> {
    let file = path(state, provider);
    std::fs::create_dir_all(file.parent().unwrap_or(state))?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(live)?)?;
    std::fs::rename(tmp, file)
}

/// Seconds since the Unix epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
