//! A selected card's settings panel as data: title, model (Automatic or one of its kind, those that can't take
//! what is wired dimmed with the reason), format (inherited or its own), length, resolution, voice, "Use the
//! project's style", its inputs numbered in `inputs_of` order, and the request preview; and the
//! `update_node` op each change makes. The models, voices and the preview come from otto through the view.

use serde_json::{Value, json};
use silo_proto::studio::{Canvas, EdgeRole, GenKind, NodeBody, Op, Project, Section};

use super::apply;

/// A model the view lists (from otto's catalog), as far as the panel needs it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelInfo {
    /// `provider/model`.
    pub id: String,
    pub label: String,
    pub makes: GenKind,
    /// The ports it can take inputs through (`text` always works: it goes into the prompt).
    pub ports: Vec<EdgeRole>,
    /// False: it needs setup (`requires` says what).
    pub available: bool,
    pub requires: Option<String>,
    /// What it offers; empty: anything.
    pub aspects: Vec<String>,
    pub durations: Vec<f64>,
    pub resolutions: Vec<String>,
}

/// A voice for speech cards.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceInfo {
    pub id: String,
    pub label: String,
}

/// otto's compiled request for the card, without running it (§6.2's preview).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    pub prompt: String,
    pub model: Option<String>,
    /// The files sent, in order.
    pub files: Vec<String>,
    /// `16:9 · 1080p · 5 s`.
    pub size: Option<String>,
    /// What stops it from running (`Run Keyframe 3 first`).
    pub error: Option<String>,
}

/// What the view passes in.
#[derive(Clone, Copy, Debug, Default)]
pub struct SettingsInput<'a> {
    pub project: Option<&'a Project>,
    pub sections: &'a [Section],
    /// Every model of every kind (filtered here by the card's).
    pub models: &'a [ModelInfo],
    /// The model Automatic would run with now.
    pub automatic: Option<&'a str>,
    pub voices: &'a [VoiceInfo],
    pub preview: Option<&'a Preview>,
}

/// An option of a choice field.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// `None`: inherited / automatic / unset.
    pub value: Option<String>,
    pub label: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub selected: bool,
}

/// A field picking one of `choices` (the first is the inherited or automatic one).
#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceField {
    pub choices: Vec<Choice>,
}

impl ChoiceField {
    #[must_use]
    pub fn selected(&self) -> Option<&Choice> {
        self.choices.iter().find(|c| c.selected)
    }
}

/// A wired input, numbered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputRow {
    pub edge: String,
    /// The source node (click: jump to it).
    pub from: String,
    pub port: EdgeRole,
    /// 1-based, in `inputs_of` order (the order the model gets them).
    pub n: usize,
    /// 1-based within its port: `Reference 2`.
    pub in_port: usize,
    /// `Reference 2 · Mara Venn`.
    pub label: String,
    /// Why it can't be used now (`Run Keyframe 3 first`, `Mara has no sheet yet`).
    pub problem: Option<String>,
    /// Still running: the run waits for it.
    pub pending: bool,
}

/// A card's settings panel.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSettings {
    pub node: String,
    pub makes: GenKind,
    /// Its own title (empty: none).
    pub title: String,
    /// What it shows without one.
    pub title_placeholder: String,
    pub model: ChoiceField,
    /// Inherited (`Inherited · 9:16`) or an aspect of its own.
    pub format: Option<ChoiceField>,
    /// Seconds.
    pub length: Option<ChoiceField>,
    pub resolution: Option<ChoiceField>,
    pub voice: Option<ChoiceField>,
    /// "Use the project's style" (images and video; on unless `options.style` is false).
    pub style: Option<bool>,
    pub inputs: Vec<InputRow>,
    pub preview: Option<Preview>,
}

/// A change made in the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingChange {
    Title(String),
    /// `None`: Automatic.
    Model(Option<String>),
    /// `None`: inherited.
    Aspect(Option<String>),
    Duration(Option<f64>),
    Resolution(Option<String>),
    Voice(Option<String>),
    Style(bool),
    /// One of the model's own options (its input schema's top-level field `key`); `null` unsets it.
    Option(String, Value),
}

/// The `update_node` op a change makes (a merge patch: `null` clears back to inherited).
#[must_use]
pub fn change_op(id: &str, change: &SettingChange) -> Op {
    let opt = |k: &str, v: Value| json!({ "options": { k: v } });
    let s = |v: &Option<String>| v.as_ref().map_or(Value::Null, |s| Value::String(s.clone()));
    let body = match change {
        SettingChange::Title(t) => json!({ "title": if t.trim().is_empty() { Value::Null } else { Value::String(t.trim().to_owned()) } }),
        SettingChange::Model(m) => json!({ "model": s(m), "mode": Value::Null }),
        SettingChange::Aspect(a) => opt("aspect_ratio", s(a)),
        SettingChange::Duration(d) => opt("duration", d.map_or(Value::Null, |d| json!(d))),
        SettingChange::Resolution(r) => opt("resolution", s(r)),
        SettingChange::Voice(v) => opt("voice", s(v)),
        // On is the default: unset it.
        SettingChange::Style(on) => opt("style", if *on { Value::Null } else { Value::Bool(false) }),
        SettingChange::Option(k, v) => opt(k, v.clone()),
    };
    apply::patch(id, body)
}

fn choice(value: Option<String>, label: String, current: Option<&str>) -> Choice {
    let selected = value.as_deref() == current;
    Choice { value, label, enabled: true, reason: None, selected }
}

/// `5` → `5 s`, `2.5` → `2.5 s`.
fn secs(d: f64) -> String {
    if (d - d.round()).abs() < 1e-9 { format!("{d:.0} s") } else { format!("{d} s") }
}

/// The settings of card `id`; `None` when it isn't a card.
#[must_use]
pub fn card_settings(canvas: &Canvas, id: &str, input: &SettingsInput<'_>) -> Option<CardSettings> {
    let node = canvas.node(id)?;
    let NodeBody::Gen { makes, title, model, options, .. } = &node.body else { return None };
    let makes = *makes;
    let opt_str = |k: &str| options.get(k).and_then(Value::as_str).map(str::to_owned);
    let wires = canvas.inputs_of(id);
    let wired: Vec<EdgeRole> = wires.iter().map(|e| e.role).filter(|r| *r != EdgeRole::Text).collect();

    let mut models = vec![Choice {
        value: None,
        label: input.automatic.map_or_else(|| "Automatic".to_owned(), |m| format!("Automatic · {}", m.rsplit_once('/').map_or(m, |(_, s)| s))),
        enabled: true,
        reason: None,
        selected: model.is_none(),
    }];
    for m in input.models.iter().filter(|m| m.makes == makes) {
        let cant: Vec<&str> = wired.iter().filter(|r| !m.ports.contains(r)).map(|r| r.name()).collect();
        let reason = if !m.available {
            Some(m.requires.clone().map_or_else(|| "Not set up".to_owned(), |r| format!("Needs {r}")))
        } else if !cant.is_empty() {
            let mut cant = cant;
            cant.dedup();
            Some(format!("Can't take {}", cant.join(", ")))
        } else {
            None
        };
        models.push(Choice {
            value: Some(m.id.clone()),
            label: m.label.clone(),
            enabled: reason.is_none(),
            reason,
            selected: model.as_deref() == Some(m.id.as_str()),
        });
    }
    if let Some(m) = model.as_deref().filter(|m| !input.models.iter().any(|x| x.id == *m)) {
        models.push(Choice { value: Some(m.to_owned()), label: m.to_owned(), enabled: true, reason: None, selected: true });
    }
    let in_effect = model.as_deref().or(input.automatic).and_then(|m| input.models.iter().find(|x| x.id == m));

    let effective = input.project.and_then(|p| p.format_for(canvas, node.parent.as_deref()));
    let picture = matches!(makes, GenKind::Image | GenKind::Video);
    let format = picture.then(|| {
        let own = opt_str("aspect_ratio");
        let inherited = effective.as_ref().map_or_else(|| "Inherited".to_owned(), |(name, f)| format!("Inherited · {} ({name})", f.aspect.trim()));
        let mut aspects: Vec<String> = in_effect.map(|m| m.aspects.clone()).filter(|a| !a.is_empty()).unwrap_or_else(|| {
            let mut a: Vec<String> = input.project.map(|p| p.formats.values().map(|f| f.aspect.trim().to_owned()).collect()).unwrap_or_default();
            a.extend(["16:9", "9:16", "1:1", "4:5"].map(str::to_owned));
            a
        });
        if let Some(o) = &own {
            aspects.push(o.clone());
        }
        let mut seen = std::collections::HashSet::new();
        aspects.retain(|a| seen.insert(a.clone()));
        let mut choices = vec![choice(None, inherited, own.as_deref())];
        choices.extend(aspects.into_iter().map(|a| choice(Some(a.clone()), a, own.as_deref())));
        ChoiceField { choices }
    });

    let length = matches!(makes, GenKind::Video | GenKind::Sound | GenKind::Music).then(|| {
        let now = options.get("duration").and_then(Value::as_f64);
        let mut ds: Vec<f64> = in_effect.map(|m| m.durations.clone()).unwrap_or_default();
        if ds.is_empty() {
            ds = vec![5.0, 10.0];
        }
        if let Some(d) = now.filter(|d| !ds.iter().any(|x| (x - d).abs() < 1e-9)) {
            ds.push(d);
        }
        let mut choices = vec![Choice { value: None, label: "Model default".into(), enabled: true, reason: None, selected: now.is_none() }];
        choices.extend(ds.into_iter().map(|d| Choice {
            value: Some(d.to_string()),
            label: secs(d),
            enabled: true,
            reason: None,
            selected: now.is_some_and(|n| (n - d).abs() < 1e-9),
        }));
        ChoiceField { choices }
    });

    let resolution = picture.then(|| {
        let own = opt_str("resolution");
        let inherited =
            effective.as_ref().and_then(|(_, f)| f.resolution.clone()).map_or_else(|| "Inherited".to_owned(), |r| format!("Inherited · {r}"));
        let mut rs: Vec<String> = in_effect
            .map(|m| m.resolutions.clone())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| ["480p", "720p", "1080p"].map(str::to_owned).to_vec());
        if let Some(o) = own.as_ref().filter(|o| !rs.contains(o)) {
            rs.push(o.clone());
        }
        let mut choices = vec![choice(None, inherited, own.as_deref())];
        choices.extend(rs.into_iter().map(|r| choice(Some(r.clone()), r, own.as_deref())));
        ChoiceField { choices }
    });

    let voice = (makes == GenKind::Speech).then(|| {
        let own = opt_str("voice");
        let mut choices = vec![choice(None, "Default voice".into(), own.as_deref())];
        choices.extend(input.voices.iter().map(|v| choice(Some(v.id.clone()), v.label.clone(), own.as_deref())));
        if let Some(o) = own.as_ref().filter(|o| !input.voices.iter().any(|v| &v.id == *o)) {
            choices.push(choice(Some(o.clone()), o.clone(), Some(o)));
        }
        ChoiceField { choices }
    });

    let style = picture.then(|| options.get("style").and_then(Value::as_bool) != Some(false));

    let mut inputs = Vec::new();
    for (n, e) in wires.iter().enumerate() {
        let in_port = wires[..n].iter().filter(|w| w.role == e.role).count() + 1;
        let out = canvas.output_of(&e.from, input.sections);
        let source = canvas.label_of(&e.from, input.sections);
        let port = e.role.name();
        let mut cap = port.chars();
        let port = cap.next().map_or_else(String::new, |f| f.to_uppercase().chain(cap).collect::<String>());
        let label = if e.role.single() { format!("{port} · {source}") } else { format!("{port} {in_port} · {source}") };
        inputs.push(InputRow {
            edge: e.id.clone(),
            from: e.from.clone(),
            port: e.role,
            n: n + 1,
            in_port,
            label,
            problem: out.as_ref().and_then(|o| o.problem.clone()),
            pending: out.is_some_and(|o| o.pending),
        });
    }

    Some(CardSettings {
        node: id.to_owned(),
        makes,
        title: title.clone().unwrap_or_default(),
        title_placeholder: format!("{} {}", makes.label(), node.id),
        model: ChoiceField { choices: models },
        format,
        length,
        resolution,
        voice,
        style,
        inputs,
        preview: input.preview.cloned(),
    })
}
