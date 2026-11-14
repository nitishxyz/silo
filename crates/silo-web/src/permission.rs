//! Pages asking for the camera, microphone, location or notifications:
//! the app asks the user (`Event::PermissionAsked` → `Browser::answer_permission`),
//! and "always" / "never" are remembered per origin in the profile's
//! `silo-permissions.json`. Other kinds keep CEF's default; agents' pages
//! are never asked for and never get any.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use cef::*;
use serde::{Deserialize, Serialize};

use crate::shared::{Event, Shared, lock};

/// The file in a profile's directory holding its decisions.
pub const FILE: &str = "silo-permissions.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Camera,
    Microphone,
    Location,
    Notifications,
}

impl Permission {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Microphone => "microphone",
            Self::Location => "location",
            Self::Notifications => "notifications",
        }
    }

    /// Chromium's content setting for it (forgotten after an "allow once").
    fn setting(self) -> ContentSettingTypes {
        match self {
            Self::Camera => ContentSettingTypes::MEDIASTREAM_CAMERA,
            Self::Microphone => ContentSettingTypes::MEDIASTREAM_MIC,
            Self::Location => ContentSettingTypes::GEOLOCATION,
            Self::Notifications => ContentSettingTypes::NOTIFICATIONS,
        }
    }
}

const PROMPT_KINDS: [(u32, Permission); 4] = [
    (sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_CAMERA_STREAM as u32, Permission::Camera),
    (sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_MIC_STREAM as u32, Permission::Microphone),
    (sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_GEOLOCATION as u32, Permission::Location),
    (sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_NOTIFICATIONS as u32, Permission::Notifications),
];

const MEDIA_KINDS: [(u32, Permission); 2] = [
    (sys::cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE as u32, Permission::Camera),
    (sys::cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DEVICE_AUDIO_CAPTURE as u32, Permission::Microphone),
];

/// The kinds in `bits`, or `None` when it has one not in `known` (or none).
fn kinds(bits: u32, known: &[(u32, Permission)]) -> Option<Vec<Permission>> {
    let all = known.iter().fold(0, |acc, (bit, _)| acc | bit);
    if bits == 0 || bits & !all != 0 {
        return None;
    }
    Some(known.iter().filter(|(bit, _)| bits & bit != 0).map(|(_, p)| *p).collect())
}

/// What the user said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Allow this time (until the page navigates or closes).
    Once,
    /// Allow, and remember it for the site.
    Always,
    /// Don't allow, and remember it for the site.
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
}

/// A page waiting for the user's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    pub id: u64,
    /// The requesting origin, e.g. `https://example.com`.
    pub origin: String,
    pub permissions: Vec<Permission>,
}

impl PermissionRequest {
    /// The origin's host (with its port), or the origin itself.
    #[must_use]
    pub fn site(&self) -> String {
        site(&self.origin)
    }

    /// "example.com wants to use your camera and microphone".
    #[must_use]
    pub fn text(&self) -> String {
        format!("{} wants to {}", self.site(), wants(&self.permissions))
    }
}

fn site(origin: &str) -> String {
    let rest = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty() { origin.to_owned() } else { host.to_owned() }
}

fn wants(permissions: &[Permission]) -> String {
    let has = |p| permissions.contains(&p);
    let mut parts = Vec::new();
    match (has(Permission::Camera), has(Permission::Microphone)) {
        (true, true) => parts.push("use your camera and microphone"),
        (true, false) => parts.push("use your camera"),
        (false, true) => parts.push("use your microphone"),
        (false, false) => {}
    }
    if has(Permission::Location) {
        parts.push("know your location");
    }
    if has(Permission::Notifications) {
        parts.push("show notifications");
    }
    parts.join(" and ")
}

/// `origin` as decisions are keyed: lowercase, no trailing slash.
#[must_use]
pub fn origin_key(origin: &str) -> String {
    origin.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Remembered decisions, by origin and permission.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decisions {
    #[serde(default)]
    sites: BTreeMap<String, BTreeMap<Permission, Decision>>,
}

impl Decisions {
    /// What `origin` gets for all of `permissions`: denied if any is,
    /// allowed if every one is, else `None` (ask).
    #[must_use]
    pub fn decide(&self, origin: &str, permissions: &[Permission]) -> Option<Decision> {
        let site = self.sites.get(&origin_key(origin))?;
        let found: Vec<Option<&Decision>> = permissions.iter().map(|p| site.get(p)).collect();
        if found.contains(&Some(&Decision::Deny)) {
            return Some(Decision::Deny);
        }
        (!permissions.is_empty() && found.iter().all(|d| *d == Some(&Decision::Allow))).then_some(Decision::Allow)
    }

    /// Remember `answer` for `origin`; whether anything changed (`Once` never does).
    pub fn record(&mut self, origin: &str, permissions: &[Permission], answer: Answer) -> bool {
        let decision = match answer {
            Answer::Once => return false,
            Answer::Always => Decision::Allow,
            Answer::Never => Decision::Deny,
        };
        let site = self.sites.entry(origin_key(origin)).or_default();
        let mut changed = false;
        for p in permissions {
            changed |= site.insert(*p, decision) != Some(decision);
        }
        changed
    }

    /// Forget everything remembered for `origin`.
    pub fn forget(&mut self, origin: &str) -> bool {
        self.sites.remove(&origin_key(origin)).is_some()
    }

    /// The decisions in `path`; none when it is missing or unreadable.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    /// Write to `path` (temp file + rename, owner-only like the profile).
    ///
    /// # Errors
    /// Write failure.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// A profile's decisions and where they are kept.
#[derive(Debug)]
pub(crate) struct SiteStore {
    path: Option<PathBuf>,
    /// The agents' profile: nothing is ever asked for or granted.
    deny_all: bool,
    decisions: Mutex<Decisions>,
}

impl SiteStore {
    pub(crate) fn open(dir: &Path, deny_all: bool) -> Arc<Self> {
        let path = dir.join(FILE);
        let decisions = Decisions::load(&path);
        Arc::new(Self { path: Some(path), deny_all, decisions: Mutex::new(decisions) })
    }

    fn decide(&self, origin: &str, permissions: &[Permission]) -> Option<Decision> {
        if self.deny_all {
            return Some(Decision::Deny);
        }
        lock(&self.decisions).decide(origin, permissions)
    }

    fn record(&self, origin: &str, permissions: &[Permission], answer: Answer) {
        let mut decisions = lock(&self.decisions);
        if decisions.record(origin, permissions, answer)
            && let Some(path) = &self.path
            && let Err(error) = decisions.save(path)
        {
            tracing::warn!("silo-web: can't save site permissions: {error:#}");
        }
    }
}

/// How a waiting request is answered.
pub(crate) enum Reply {
    Prompt(PermissionPromptCallback),
    /// The media callback and the bits it asked for.
    Media(MediaAccessCallback, u32),
}

/// How a request ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Allow,
    /// The user or a remembered decision said no.
    Deny,
    /// Nobody answered (the page went, an agent's page): not granted, and
    /// nothing for Chromium to remember.
    Drop,
}

impl Reply {
    fn send(self, outcome: Outcome) {
        match (self, outcome) {
            (Self::Prompt(callback), Outcome::Allow) => callback.cont(PermissionRequestResult::ACCEPT),
            (Self::Prompt(callback), Outcome::Deny) => callback.cont(PermissionRequestResult::DENY),
            (Self::Prompt(callback), Outcome::Drop) => callback.cont(PermissionRequestResult::IGNORE),
            (Self::Media(callback, bits), Outcome::Allow) => callback.cont(bits),
            (Self::Media(callback, _), _) => callback.cancel(),
        }
    }
}

pub(crate) struct Pending {
    request: PermissionRequest,
    /// CEF's prompt id (`on_show_permission_prompt`), for its dismissal.
    prompt: Option<u64>,
    reply: Reply,
}

/// A grant to forget once the document goes: an "allow once".
pub(crate) struct Once {
    origin: String,
    permissions: Vec<Permission>,
}

/// The kinds a permission prompt asks for, if all are handled here.
pub(crate) fn prompt_kinds(bits: u32) -> Option<Vec<Permission>> {
    kinds(bits, &PROMPT_KINDS)
}

/// The kinds a media request asks for, if all are handled here.
pub(crate) fn media_kinds(bits: u32) -> Option<Vec<Permission>> {
    kinds(bits, &MEDIA_KINDS)
}

/// Answer a page's request at once if it can be, else ask the app.
pub(crate) fn ask(shared: &Shared, origin: &str, permissions: Vec<Permission>, prompt: Option<u64>, reply: Reply) {
    if shared.deny_prompts.load(Ordering::Acquire) {
        return reply.send(Outcome::Drop);
    }
    let store = lock(&shared.sites).clone();
    match store.as_ref().and_then(|s| s.decide(origin, &permissions)) {
        Some(Decision::Allow) => return reply.send(Outcome::Allow),
        Some(Decision::Deny) if store.as_ref().is_some_and(|s| s.deny_all) => return reply.send(Outcome::Drop),
        Some(Decision::Deny) => return reply.send(Outcome::Deny),
        None => {}
    }
    let id = shared.next_permission.fetch_add(1, Ordering::Relaxed);
    let request = PermissionRequest { id, origin: origin_key(origin), permissions };
    lock(&shared.permissions).push(Pending { request: request.clone(), prompt, reply });
    shared.push(Event::PermissionAsked(request));
}

/// The user answered request `id`.
pub(crate) fn answer(shared: &Shared, id: u64, answer: Answer) {
    let pending = {
        let mut pending = lock(&shared.permissions);
        let Some(at) = pending.iter().position(|p| p.request.id == id) else { return };
        pending.remove(at)
    };
    let request = &pending.request;
    if let Some(store) = lock(&shared.sites).clone() {
        store.record(&request.origin, &request.permissions, answer);
    }
    if answer == Answer::Once {
        lock(&shared.once).push(Once { origin: request.origin.clone(), permissions: request.permissions.clone() });
    }
    pending.reply.send(if answer == Answer::Never { Outcome::Deny } else { Outcome::Allow });
}

/// CEF dismissed prompt `prompt` itself (navigation, close).
pub(crate) fn dismissed(shared: &Shared, prompt: u64) {
    let gone = {
        let mut pending = lock(&shared.permissions);
        let Some(at) = pending.iter().position(|p| p.prompt == Some(prompt)) else { return };
        pending.remove(at)
    };
    shared.push(Event::PermissionDismissed(gone.request.id));
    // CEF no longer waits on it: dropping the callback answers nothing.
    drop(gone);
}

/// The document went (navigation, close): requests still waiting are
/// dismissed and refused, and "allow once" grants are forgotten.
pub(crate) fn document_gone(shared: &Shared, context: Option<RequestContext>) {
    let waiting: Vec<Pending> = std::mem::take(&mut *lock(&shared.permissions));
    for pending in waiting {
        shared.push(Event::PermissionDismissed(pending.request.id));
        pending.reply.send(Outcome::Drop);
    }
    let once: Vec<Once> = std::mem::take(&mut *lock(&shared.once));
    let Some(context) = context else { return };
    for grant in once {
        let origin = CefString::from(grant.origin.as_str());
        for permission in grant.permissions {
            context.set_content_setting(Some(&origin), Some(&origin), permission.setting(), ContentSettingValues::DEFAULT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAMERA: &[Permission] = &[Permission::Camera];

    #[test]
    fn decisions_are_per_origin_and_permission() {
        let mut d = Decisions::default();
        assert_eq!(d.decide("https://a.com", CAMERA), None, "nothing remembered: ask");
        assert!(!d.record("https://a.com", CAMERA, Answer::Once), "once is not remembered");
        assert_eq!(d.decide("https://a.com", CAMERA), None);
        assert!(d.record("https://a.com/", CAMERA, Answer::Always));
        assert!(!d.record("https://a.com", CAMERA, Answer::Always), "unchanged");
        assert_eq!(d.decide("HTTPS://A.COM", CAMERA), Some(Decision::Allow), "origins compare normalized");
        assert_eq!(d.decide("https://b.com", CAMERA), None, "another origin");
        assert_eq!(d.decide("http://a.com", CAMERA), None, "another scheme");
        assert_eq!(d.decide("https://a.com", &[Permission::Microphone]), None, "another permission");
        assert_eq!(d.decide("https://a.com", &[Permission::Camera, Permission::Microphone]), None, "one of two remembered: ask");
        assert!(d.record("https://a.com", &[Permission::Microphone], Answer::Never));
        assert_eq!(d.decide("https://a.com", &[Permission::Camera, Permission::Microphone]), Some(Decision::Deny), "any denied: denied");
        assert!(d.record("https://a.com", CAMERA, Answer::Never), "never replaces always");
        assert_eq!(d.decide("https://a.com", CAMERA), Some(Decision::Deny));
        assert!(d.forget("https://a.com"));
        assert_eq!(d.decide("https://a.com", CAMERA), None);
    }

    #[test]
    fn decisions_round_trip_through_their_file() {
        let dir = std::env::temp_dir().join(format!("silo-web-permissions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        assert_eq!(Decisions::load(&path), Decisions::default(), "a missing file is no decisions");
        let mut d = Decisions::default();
        d.record("https://maps.example", &[Permission::Location], Answer::Always);
        d.record("https://spam.example", &[Permission::Notifications], Answer::Never);
        d.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""location": "allow""#) && text.contains(r#""notifications": "deny""#), "{text}");
        let back = Decisions::load(&path);
        assert_eq!(back, d);
        assert_eq!(back.decide("https://maps.example", &[Permission::Location]), Some(Decision::Allow));
        std::fs::write(&path, b"{").unwrap();
        assert_eq!(Decisions::load(&path), Decisions::default(), "an unreadable file is no decisions");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_known_kinds_are_handled() {
        let camera = sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_CAMERA_STREAM as u32;
        let mic = sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_MIC_STREAM as u32;
        let clipboard = sys::cef_permission_request_types_t::CEF_PERMISSION_TYPE_CLIPBOARD as u32;
        assert_eq!(prompt_kinds(camera | mic), Some(vec![Permission::Camera, Permission::Microphone]));
        assert_eq!(prompt_kinds(clipboard), None);
        assert_eq!(prompt_kinds(camera | clipboard), None, "a kind not handled here keeps the default");
        assert_eq!(prompt_kinds(0), None);
        let video = sys::cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DEVICE_VIDEO_CAPTURE as u32;
        let desktop = sys::cef_media_access_permission_types_t::CEF_MEDIA_PERMISSION_DESKTOP_VIDEO_CAPTURE as u32;
        assert_eq!(media_kinds(video), Some(vec![Permission::Camera]));
        assert_eq!(media_kinds(video | desktop), None, "screen capture keeps the default");
    }

    #[test]
    fn requests_say_who_wants_what() {
        let request = |origin: &str, permissions: Vec<Permission>| PermissionRequest { id: 1, origin: origin.into(), permissions };
        assert_eq!(request("https://example.com", vec![Permission::Camera]).text(), "example.com wants to use your camera");
        assert_eq!(
            request("https://meet.example:8443", vec![Permission::Camera, Permission::Microphone]).text(),
            "meet.example:8443 wants to use your camera and microphone"
        );
        assert_eq!(request("https://a.b", vec![Permission::Location]).text(), "a.b wants to know your location");
        assert_eq!(request("https://a.b", vec![Permission::Notifications]).text(), "a.b wants to show notifications");
    }
}
