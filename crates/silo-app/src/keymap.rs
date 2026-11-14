//! User-remappable shortcuts. Every [`Command`] has default chords
//! ([`Command::default_keys`]); `keybindings.json` beside `terminal.json`
//! overrides them by command id ([`Command::id`]):
//!
//! ```json
//! { "NewTab": ["cmd-t", "ctrl-alt-t"], "SplitRight": "cmd-e", "HideSilo": [] }
//! ```
//!
//! a list replaces the defaults (a second chord adds one), `[]` or `null`
//! unbinds. The home server's `init.lua` (`docs/LUA.md` §4.1, §6) goes on
//! top: its unbinds, then its bindings (sequences, modes, methods and
//! actions too), winning over both. The merged keymap is the one source the
//! key bindings, the menus, the palette, tooltips and Settings › Keyboard read.

mod chord;

use std::{
    collections::{BTreeMap, HashMap},
    io::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{LazyLock, RwLock},
};

pub use chord::{Chord, ChordExt};
use gpui::{App, KeyBinding, KeyBindingContextPredicate};
use serde_json::Value;
use silo_proto::{
    config::{BindTarget, ConfigState, KeyMode},
    keys::Sequence,
};

use crate::commands::Command;

pub const FILE: &str = "keybindings.json";

/// In the workspace's key context while the top mode is exclusive: the
/// bindings outside modes are off.
pub const EXCLUSIVE: &str = "silo_exclusive";

/// In the workspace's key context for every declared mode not on top.
/// A mode's bindings are `!silo_off_<mode>`: a negation matches at the
/// deepest context, so they outrank the bindings outside modes (which have
/// no context, or `!silo_exclusive`), and being added last wins the tie.
#[must_use]
pub fn mode_off(mode: &str) -> String {
    format!("silo_off_{mode}")
}

/// The file's entries as written: command id → chords (unparsed).
pub type Overrides = BTreeMap<String, Vec<String>>;

/// What an `init.lua` binding does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Command(Command),
    /// A control method; `app.*` runs here.
    Method(String),
    Action(String),
    /// `silo.unbind("mode/keys")`: the key does nothing in that mode.
    Blocked,
}

/// One `init.lua` binding, resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    pub mode: Option<String>,
    pub keys: Sequence,
    pub target: Target,
    pub args: Value,
}

/// When a gpui binding applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum When {
    Always,
    /// Not while an exclusive mode is on top.
    NotExclusive,
    /// While this mode is on top.
    Mode(String),
}

/// What a gpui binding dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Does {
    Command(Command),
    /// [`RunBinding`] of `Keymap::config[index]`.
    Run(usize),
}

/// One gpui key binding to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub keys: String,
    pub does: Does,
    pub when: When,
}

/// Runs `init.lua` binding `index` of the current keymap (`config.rs`).
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = silo, no_json)]
pub struct RunBinding {
    pub index: usize,
}

/// Defaults merged with the overrides, then with `init.lua`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    /// Defaults with `keybindings.json`: what Settings › Keyboard edits.
    pub bindings: HashMap<Command, Vec<Chord>>,
    /// Unknown ids, bad chords and conflicts, for Settings › Keyboard.
    pub problems: Vec<String>,
    /// `init.lua`'s bindings, in order ([`RunBinding`]'s index).
    pub config: Vec<Bound>,
    /// Chords of `bindings` that `init.lua` unbound or took: not bound.
    pub taken: Vec<(Command, Chord)>,
    pub modes: Vec<KeyMode>,
    /// `init.lua`'s path, once the home server answered `config.get`.
    pub config_path: Option<String>,
}

impl Keymap {
    /// A command's shortcuts as they act: `init.lua`'s first, then the
    /// chords it left. The first is the hint.
    #[must_use]
    pub fn shortcuts(&self, command: Command) -> Vec<Sequence> {
        let mut out = self.config_shortcuts(command);
        for chord in self.bindings.get(&command).into_iter().flatten() {
            let sequence = Sequence(vec![chord.clone()]);
            if !self.taken.contains(&(command, chord.clone())) && !out.contains(&sequence) {
                out.push(sequence);
            }
        }
        out
    }

    /// The shortcuts `init.lua` gives `command` outside modes.
    #[must_use]
    pub fn config_shortcuts(&self, command: Command) -> Vec<Sequence> {
        let mut out: Vec<Sequence> = Vec::new();
        for bound in self.config.iter().filter(|b| b.mode.is_none() && b.target == Target::Command(command)) {
            if !out.contains(&bound.keys) {
                out.push(bound.keys.clone());
            }
        }
        out
    }

    /// The hint of the first binding outside modes running action `name`.
    #[must_use]
    pub fn action_hint(&self, name: &str) -> Option<String> {
        self.config.iter().find(|b| b.mode.is_none() && matches!(&b.target, Target::Action(a) if a == name)).map(|b| b.keys.hint())
    }

    /// How many shortcuts `init.lua` adds (blocked keys aren't any).
    #[must_use]
    pub fn config_count(&self) -> usize {
        self.config.iter().filter(|b| b.target != Target::Blocked).count()
    }

    /// Whether `init.lua` binds (or blocks) keys starting with `chord`
    /// outside modes: such a chord is the config's, not a pane's own
    /// (a page's ⌘R).
    #[must_use]
    pub fn config_starts(&self, chord: &Chord) -> bool {
        self.config.iter().any(|b| b.mode.is_none() && b.keys.0.first() == Some(chord))
    }

    /// Whether `mode` binds `keys` (or blocks them).
    #[must_use]
    pub fn binds(&self, mode: &str, keys: &Sequence) -> bool {
        self.config.iter().any(|b| b.mode.as_deref() == Some(mode) && b.keys == *keys)
    }

    /// The gpui bindings, in the order to add them: `init.lua`'s commands
    /// first (a menu shows a command's first binding), then the chords left
    /// of the defaults and `keybindings.json`, then `init.lua`'s methods
    /// and actions, then the modes' (last: they win ties).
    #[must_use]
    pub fn specs(&self) -> Vec<Spec> {
        let normal = if self.modes.iter().any(|m| m.exclusive) { When::NotExclusive } else { When::Always };
        let mut out = Vec::new();
        for bound in self.config.iter().filter(|b| b.mode.is_none()) {
            if let Target::Command(command) = bound.target {
                out.push(Spec { keys: bound.keys.to_string(), does: Does::Command(command), when: normal.clone() });
            }
        }
        for command in Command::ALL {
            for chord in self.bindings.get(command).into_iter().flatten() {
                if !self.taken.contains(&(*command, chord.clone())) {
                    out.push(Spec { keys: chord.to_string(), does: Does::Command(*command), when: normal.clone() });
                }
            }
        }
        for (index, bound) in self.config.iter().enumerate().filter(|(_, b)| b.mode.is_none()) {
            if !matches!(bound.target, Target::Command(_)) {
                out.push(Spec { keys: bound.keys.to_string(), does: Does::Run(index), when: normal.clone() });
            }
        }
        for (index, bound) in self.config.iter().enumerate() {
            if let Some(mode) = &bound.mode {
                out.push(Spec { keys: bound.keys.to_string(), does: Does::Run(index), when: When::Mode(mode.clone()) });
            }
        }
        out
    }
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
    Keymap { bindings, problems, ..Keymap::default() }
}

/// `source: ` before a message about it, when there is one.
fn at(source: &str) -> String {
    if source.is_empty() { String::new() } else { format!("{source}: ") }
}

/// `keymap` with the home server's `init.lua` on top: its unbinds, then
/// its bindings, which win over every other binding of their keys (noted).
#[must_use]
pub fn with_config(mut keymap: Keymap, config: Option<&ConfigState>) -> Keymap {
    let Some(config) = config else { return keymap };
    keymap.config_path = Some(config.path.clone());
    keymap.modes.clone_from(&config.modes);
    let mut problems: Vec<String> = config.problems.iter().map(|p| format!("{}{}", at(&p.source), p.message)).collect();
    let known = |mode: &str| config.modes.iter().any(|m| m.name == mode);
    let mut taken: Vec<(Command, Chord)> = Vec::new();
    let mut out: Vec<Bound> = Vec::new();
    for unbind in &config.unbinds {
        let at = at(&unbind.source);
        let keys = match Sequence::parse(&unbind.keys) {
            Ok(keys) => keys,
            Err(error) => {
                problems.push(format!("{at}{error}"));
                continue;
            }
        };
        match &unbind.mode {
            Some(mode) if !known(mode) => problems.push(format!("{at}Unknown mode “{mode}”")),
            Some(mode) => {
                if !out.iter().any(|b| b.mode.as_deref() == Some(mode) && b.keys == keys) {
                    out.push(Bound { mode: Some(mode.clone()), keys, target: Target::Blocked, args: Value::Null });
                }
            }
            None => {
                // `keybindings.json` has single chords only.
                let [chord] = keys.0.as_slice() else { continue };
                let only = unbind.target.as_deref();
                for command in Command::ALL.iter().filter(|c| only.is_none_or(|t| t == c.id())) {
                    if keymap.bindings.get(command).is_some_and(|keys| keys.contains(chord)) && !taken.contains(&(*command, chord.clone())) {
                        taken.push((*command, chord.clone()));
                    }
                }
            }
        }
    }
    for binding in &config.bindings {
        let at = at(&binding.source);
        let keys = match Sequence::parse(&binding.keys) {
            Ok(keys) => keys,
            Err(error) => {
                problems.push(format!("{at}{error}"));
                continue;
            }
        };
        if let Some(mode) = binding.mode.as_deref().filter(|m| !known(m)) {
            problems.push(format!("{at}Unknown mode “{mode}”"));
            continue;
        }
        let target = match &binding.target {
            BindTarget::Command { id } => match Command::from_id(id) {
                Some(command) => Target::Command(command),
                None => {
                    problems.push(format!("{at}Unknown command “{id}”"));
                    continue;
                }
            },
            BindTarget::Method { name } => Target::Method(name.clone()),
            BindTarget::Action { name } => Target::Action(name.clone()),
            BindTarget::Unknown => {
                problems.push(format!("{at}{}: this version of Silo can't run what it's bound to", keys.hint()));
                continue;
            }
        };
        if binding.mode.is_none() {
            // A chord of the defaults or `keybindings.json` that is these
            // keys, or starts them, gives way.
            let first = &keys.0[0];
            let mut losers: Vec<&'static str> = Vec::new();
            for command in Command::ALL {
                if !keymap.bindings.get(command).is_some_and(|k| k.contains(first)) || taken.contains(&(*command, first.clone())) {
                    continue;
                }
                taken.push((*command, first.clone()));
                if !(keys.0.len() == 1 && target == Target::Command(*command)) {
                    losers.push(command.title().trim_end_matches('…'));
                }
            }
            if !losers.is_empty() {
                let what = if keys.0.len() == 1 { String::new() } else { format!(" {}", first.hint()) };
                problems.push(format!("{at}{} replaces {}'s{what}", keys.hint(), losers.join(" and ")));
            }
        }
        if let Some(earlier) = out.iter().position(|b| b.mode == binding.mode && b.keys == keys) {
            if out[earlier].target != Target::Blocked {
                problems.push(format!("{at}{} is bound twice; the last one wins", keys.hint()));
            }
            out.remove(earlier);
        }
        out.push(Bound { mode: binding.mode.clone(), keys, target, args: binding.args.clone() });
    }
    keymap.problems.extend(problems);
    keymap.config = out;
    keymap.taken = taken;
    keymap
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
    /// The home server's config, last answered.
    config: Option<ConfigState>,
}

impl State {
    fn remerge(&mut self, file_problem: Option<String>) {
        let mut json = merge(&self.overrides);
        json.problems.splice(0..0, file_problem);
        self.keymap = with_config(json, self.config.as_ref());
    }
}

static STATE: LazyLock<RwLock<State>> = LazyLock::new(|| RwLock::new(State { keymap: merge(&Overrides::new()), ..State::default() }));

/// A command's effective shortcuts; the first is its hint.
#[must_use]
pub fn shortcuts(command: Command) -> Vec<Sequence> {
    STATE.read().map(|s| s.keymap.shortcuts(command)).unwrap_or_default()
}

/// The shortcuts `init.lua` gives `command`.
#[must_use]
pub fn config_shortcuts(command: Command) -> Vec<Sequence> {
    STATE.read().map(|s| s.keymap.config_shortcuts(command)).unwrap_or_default()
}

/// `command`'s chords `init.lua` unbound or took.
#[must_use]
pub fn taken(command: Command) -> Vec<Chord> {
    STATE.read().map(|s| s.keymap.taken.iter().filter(|(c, _)| *c == command).map(|(_, chord)| chord.clone()).collect()).unwrap_or_default()
}

/// The shortcut of action `name`, if `init.lua` binds one.
#[must_use]
pub fn action_hint(name: &str) -> Option<String> {
    STATE.read().ok()?.keymap.action_hint(name)
}

/// `init.lua` binding `index`.
#[must_use]
pub fn bound(index: usize) -> Option<Bound> {
    STATE.read().ok()?.keymap.config.get(index).cloned()
}

/// The modes `init.lua` declares.
#[must_use]
pub fn modes() -> Vec<KeyMode> {
    STATE.read().map(|s| s.keymap.modes.clone()).unwrap_or_default()
}

/// Whether `init.lua` binds keys starting with this key press outside
/// modes ([`Keymap::config_starts`]).
#[must_use]
pub fn config_starts(keystroke: &gpui::Keystroke) -> bool {
    Chord::from_keystroke(keystroke).is_some_and(|chord| STATE.read().is_ok_and(|s| s.keymap.config_starts(&chord)))
}

/// Whether `mode` binds (or blocks) `keys`.
#[must_use]
pub fn mode_binds(mode: &str, keys: &Sequence) -> bool {
    STATE.read().is_ok_and(|s| s.keymap.binds(mode, keys))
}

/// `init.lua`'s path and how many shortcuts it adds, once the home server
/// answered.
#[must_use]
pub fn config_summary() -> Option<(String, usize)> {
    let state = STATE.read().ok()?;
    Some((state.keymap.config_path.clone()?, state.keymap.config_count()))
}

/// The effective keymap.
#[must_use]
pub fn current() -> Keymap {
    STATE.read().map(|s| s.keymap.clone()).unwrap_or_default()
}

/// A binding's context predicate.
fn predicate(when: &When) -> Option<Rc<KeyBindingContextPredicate>> {
    let not = |id: String| Rc::new(KeyBindingContextPredicate::Not(Box::new(KeyBindingContextPredicate::Identifier(id.into()))));
    match when {
        When::Always => None,
        When::NotExclusive => Some(not(EXCLUSIVE.to_owned())),
        When::Mode(mode) => Some(not(mode_off(mode))),
    }
}

/// Install the effective bindings (after ⌘1–9, so they win those keys) as
/// gpui bindings and refresh the menus.
fn bind(cx: &mut App) {
    let keymap = current();
    let mut out = Vec::new();
    for spec in keymap.specs() {
        let action: Box<dyn gpui::Action> = match spec.does {
            Does::Command(command) => command.action(),
            Does::Run(index) => Box::new(RunBinding { index }),
        };
        match KeyBinding::load(&spec.keys, action, predicate(&spec.when), false, None, &gpui::DummyKeyboardMapper) {
            Ok(binding) => out.push(binding),
            Err(error) => tracing::warn!("bad key binding {}: {error:?}", spec.keys),
        }
    }
    cx.clear_key_bindings();
    crate::workspace::bind_number_keys(cx);
    cx.bind_keys(out);
    crate::set_menus(cx);
}

/// Apply the home server's config (`None`: it has none), when it changed.
pub fn set_config(config: Option<ConfigState>, cx: &mut App) {
    let Ok(mut state) = STATE.write() else { return };
    if state.config == config {
        return;
    }
    state.config = config;
    let file_problem = state.broken.then(|| state.keymap.problems.first().cloned()).flatten();
    state.remerge(file_problem);
    drop(state);
    bind(cx);
    refresh_settings(cx);
}

/// Re-read the file and apply it.
fn reload(cx: &mut App) {
    let Ok(mut state) = STATE.write() else { return };
    let (overrides, problem) = load(&state.home);
    state.broken = problem.is_some();
    state.stamp = stamp(&state.home);
    state.overrides = overrides;
    state.remerge(problem);
    for problem in &state.keymap.problems {
        tracing::warn!("{problem}");
    }
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
    /// Open the home server's `init.lua` in a text editor (created empty).
    OpenConfig,
    /// `config.reload` on the home server.
    ReloadConfig,
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
    state.overrides = overrides;
    state.broken = false;
    state.remerge(None);
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
        KeymapEvent::OpenConfig => {
            let config = STATE.read().ok().and_then(|s| s.keymap.config_path.clone()).filter(|p| !p.is_empty());
            let config = config.map_or_else(|| silo_proto::paths::config_file(&home), PathBuf::from);
            if !config.exists() {
                let created = config.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&config, ""));
                created.map_err(|e| {
                    tracing::warn!("create {}: {e}", config.display());
                    "Couldn't create init.lua.".to_owned()
                })?;
            }
            std::process::Command::new("/usr/bin/open").arg("-t").arg(&config).spawn().map(drop).map_err(|e| {
                tracing::warn!("open init.lua: {e}");
                "Couldn't open init.lua.".to_owned()
            })
        }
        KeymapEvent::ReloadConfig => crate::config::reload_home(cx),
    }
}

#[cfg(test)]
mod tests;
