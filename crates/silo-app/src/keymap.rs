//! User-remappable shortcuts. Every [`Command`] has default chords
//! ([`Command::default_keys`]); `keybindings.json` beside `terminal.json`
//! overrides them by command id ([`Command::id`]):
//!
//! ```json
//! { "NewTab": ["cmd-t", "ctrl-alt-t"], "SplitRight": "cmd-e", "HideSilo": [] }
//! ```
//!
//! a list replaces the defaults (a second chord adds one), `[]` or `null`
//! unbinds. The merged keymap is the one source the key bindings, the
//! menus, the palette, tooltips and Settings › Keyboard read.

mod chord;

use std::{
    collections::{BTreeMap, HashMap},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{LazyLock, RwLock},
};

pub use chord::Chord;
use gpui::{App, KeyBinding};

use crate::commands::Command;

pub const FILE: &str = "keybindings.json";

/// The file's entries as written: command id → chords (unparsed).
pub type Overrides = BTreeMap<String, Vec<String>>;

/// Defaults merged with the overrides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    pub bindings: HashMap<Command, Vec<Chord>>,
    /// Unknown ids, bad chords and conflicts, for Settings › Keyboard.
    pub problems: Vec<String>,
}

pub(crate) fn dedup(chords: impl IntoIterator<Item = Chord>) -> Vec<Chord> {
    let mut out: Vec<Chord> = Vec::new();
    for chord in chords {
        if !out.contains(&chord) {
            out.push(chord);
        }
    }
    out
}

/// A command's default chords.
#[must_use]
pub fn defaults(command: Command) -> Vec<Chord> {
    dedup(command.default_keys().iter().filter_map(|k| Chord::parse(k).ok()))
}

/// Parse `keybindings.json`: each value is a chord, a list of them, or `null`.
///
/// # Errors
/// Not a JSON object of those.
pub fn parse(text: &str) -> Result<Overrides, String> {
    let map: BTreeMap<String, serde_json::Value> = serde_json::from_str(text).map_err(|e| format!("Shortcuts file is invalid ({e})"))?;
    map.into_iter()
        .map(|(id, value)| {
            let keys = match value {
                serde_json::Value::Null => Vec::new(),
                serde_json::Value::String(key) => vec![key],
                serde_json::Value::Array(keys) => keys
                    .into_iter()
                    .map(|k| k.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or_else(|| format!("{id}: expected shortcuts"))?,
                _ => return Err(format!("{id}: expected a shortcut or a list")),
            };
            Ok((id, keys))
        })
        .collect()
}

/// Defaults with `overrides` applied; problems are reported, never fatal.
#[must_use]
pub fn merge(overrides: &Overrides) -> Keymap {
    let mut bindings: HashMap<Command, Vec<Chord>> = Command::ALL.iter().map(|c| (*c, defaults(*c))).collect();
    let mut problems = Vec::new();
    for (id, keys) in overrides {
        let Some(command) = Command::from_id(id) else {
            problems.push(format!("Unknown command “{id}”"));
            continue;
        };
        let chords = keys.iter().filter_map(|k| Chord::parse(k).map_err(|e| problems.push(format!("{}: {e}", command.title()))).ok());
        bindings.insert(command, dedup(chords));
    }
    for (chord, users) in conflicts(&bindings) {
        let names: Vec<&str> = users.iter().map(|c| c.title()).collect();
        problems.push(format!("{} is used by {}", chord.hint(), names.join(" and ")));
    }
    Keymap { bindings, problems }
}

/// Chords bound to more than one command, in [`Command::ALL`] order.
#[must_use]
pub fn conflicts(bindings: &HashMap<Command, Vec<Chord>>) -> Vec<(Chord, Vec<Command>)> {
    let mut out: Vec<(Chord, Vec<Command>)> = Vec::new();
    for command in Command::ALL {
        for chord in bindings.get(command).into_iter().flatten() {
            match out.iter_mut().find(|(c, _)| c == chord) {
                Some((_, users)) => users.push(*command),
                None => out.push((chord.clone(), vec![*command])),
            }
        }
    }
    out.retain(|(_, users)| users.len() > 1);
    out
}

/// `overrides` with `command` set to `chords`: no entry when they are the defaults.
pub fn set(overrides: &mut Overrides, command: Command, chords: &[Chord]) {
    if chords == defaults(command).as_slice() {
        overrides.remove(command.id());
    } else {
        overrides.insert(command.id().to_owned(), chords.iter().map(ToString::to_string).collect());
    }
}

#[must_use]
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

/// Read `keybindings.json`; missing is no overrides. An unreadable or
/// invalid file means none (reported) until it is fixed or a shortcut is set.
fn load(home: &Path) -> (Overrides, Option<String>) {
    match std::fs::read_to_string(path(home)) {
        Ok(text) => parse(&text).map_or_else(|e| (Overrides::new(), Some(e)), |o| (o, None)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Overrides::new(), None),
        Err(e) => (Overrides::new(), Some(format!("Could not read the shortcuts file: {e}"))),
    }
}

/// Atomically replace `keybindings.json`.
fn save(home: &Path, overrides: &Overrides) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    let tmp = home.join(format!(".{FILE}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut bytes = serde_json::to_vec_pretty(overrides).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path(home))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The file's modification time and size, to notice edits by hand.
fn stamp(home: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path(home)).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[derive(Default)]
struct State {
    home: PathBuf,
    overrides: Overrides,
    keymap: Keymap,
    stamp: Option<(std::time::SystemTime, u64)>,
    /// The file could not be read or parsed: Settings won't overwrite it
    /// (losing the edits by hand) until it is fixed or defaults are restored.
    broken: bool,
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(|| RwLock::new(State { keymap: merge(&Overrides::new()), ..State::default() }));

/// A command's effective chords; the first is its hint.
#[must_use]
pub fn bindings(command: Command) -> Vec<Chord> {
    STATE.read().map(|s| s.keymap.bindings.get(&command).cloned().unwrap_or_default()).unwrap_or_default()
}

/// The effective keymap.
#[must_use]
pub fn current() -> Keymap {
    STATE.read().map(|s| s.keymap.clone()).unwrap_or_default()
}

/// Install the effective chords (plus ⌘1–9) as gpui bindings and refresh the menus.
fn bind(cx: &mut App) {
    let keymap = current();
    let mut out = Vec::new();
    for command in Command::ALL {
        for chord in keymap.bindings.get(command).into_iter().flatten() {
            match KeyBinding::load(&chord.to_string(), command.action(), None, false, None, &gpui::DummyKeyboardMapper) {
                Ok(binding) => out.push(binding),
                Err(error) => tracing::warn!("bad key binding {chord}: {error:?}"),
            }
        }
    }
    cx.clear_key_bindings();
    cx.bind_keys(out);
    crate::workspace::bind_number_keys(cx);
    crate::set_menus(cx);
}

/// Re-read the file and apply it.
fn reload(cx: &mut App) {
    let Ok(mut state) = STATE.write() else { return };
    let (overrides, problem) = load(&state.home);
    state.broken = problem.is_some();
    state.stamp = stamp(&state.home);
    state.keymap = merge(&overrides);
    state.keymap.problems.splice(0..0, problem);
    for problem in &state.keymap.problems {
        tracing::warn!("{problem}");
    }
    state.overrides = overrides;
    drop(state);
    bind(cx);
}

/// At launch: apply `home`'s file, then follow edits to it.
pub fn install(home: PathBuf, cx: &mut App) {
    if let Ok(mut state) = STATE.write() {
        state.home = home;
    }
    reload(cx);
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let changed = STATE.read().is_ok_and(|s| stamp(&s.home) != s.stamp);
            if changed {
                cx.update(|cx| {
                    reload(cx);
                    refresh_settings(cx);
                });
            }
        }
    })
    .detach();
}

/// Show the new shortcuts in every open Settings.
fn refresh_settings(cx: &mut App) {
    for window in cx.windows() {
        if let Some(handle) = window.downcast::<crate::workspace::Workspace>() {
            let _ = handle.update(cx, |workspace, _, cx| workspace.refresh_settings(None, cx));
        }
    }
}

/// Settings › Keyboard's requests.
#[derive(Clone, Debug, PartialEq)]
pub enum KeymapEvent {
    /// Give commands these chords (a replaced conflict sets two at once).
    Set(Vec<(Command, Vec<Chord>)>),
    Reset(Command),
    RestoreDefaults,
    /// Open the file in a text editor.
    OpenFile,
    RevealFile,
}

/// Change the overrides, save and apply them.
fn change(edit: impl FnOnce(&mut Overrides), cx: &mut App) -> Result<(), String> {
    let Ok(mut state) = STATE.write() else { return Err("Couldn't change the shortcut.".into()) };
    if state.broken {
        return Err("Fix the shortcuts file first, or restore defaults.".into());
    }
    let mut overrides = state.overrides.clone();
    edit(&mut overrides);
    save(&state.home, &overrides).map_err(|e| {
        tracing::warn!("save shortcuts: {e}");
        "Couldn't save the shortcuts.".to_owned()
    })?;
    state.stamp = stamp(&state.home);
    state.keymap = merge(&overrides);
    state.overrides = overrides;
    state.broken = false;
    drop(state);
    bind(cx);
    Ok(())
}

/// Apply a Settings › Keyboard request; an error to show on failure.
///
/// # Errors
/// The file could not be written (or created to be opened).
pub fn apply(event: &KeymapEvent, cx: &mut App) -> Result<(), String> {
    let home = STATE.read().map(|s| s.home.clone()).unwrap_or_default();
    let file = path(&home);
    let create = || if file.exists() { Ok(()) } else { save(&home, &Overrides::new()).map_err(|_| "Couldn't create the shortcuts file.".to_owned()) };
    match event {
        KeymapEvent::Set(changes) => change(|o| changes.iter().for_each(|(command, chords)| set(o, *command, chords)), cx),
        KeymapEvent::Reset(command) => change(|o| drop(o.remove(command.id())), cx),
        KeymapEvent::RestoreDefaults => {
            // An explicit choice: it replaces a broken file too.
            if let Ok(mut state) = STATE.write() {
                state.broken = false;
            }
            change(Overrides::clear, cx)
        }
        KeymapEvent::OpenFile => {
            create()?;
            std::process::Command::new("/usr/bin/open").arg("-t").arg(&file).spawn().map(drop).map_err(|e| {
                tracing::warn!("open shortcuts file: {e}");
                "Couldn't open the shortcuts file.".to_owned()
            })
        }
        KeymapEvent::RevealFile => {
            create()?;
            cx.reveal_path(&file);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
