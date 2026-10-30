//! Voice typing: Fn held or pressed twice dictates into the focused block,
//! on this Mac. `silo_voice` does the listening; this is the trigger's
//! wiring, the pill ([`pill`]), insertion ([`insert`]) and model
//! downloads ([`fetch`]).

pub(crate) mod fetch;
mod insert;
mod pill;
pub(crate) mod settings;

use std::time::{Duration, Instant};

use gpui::{Context, KeyDownEvent, ModifiersChangedEvent, Task, Window};
use silo_voice::{
    bands::{BANDS, BARS},
    engine::Options,
    session::{Config, Phase, Session},
    trigger::{Action, Input, Trigger},
};

use crate::{sound, status::Status, workspace::Workspace};

/// How a dictation is going, as the pill shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Recording,
    /// Stopped; transcribing (or waiting for the model) since.
    Finishing(Instant),
    /// Done or cancelled: fading out since.
    Leaving(Instant),
}

struct Live {
    session: Session,
    hands_free: bool,
    engine: silo_voice::EngineKind,
    stage: Stage,
    /// The bars as last drawn, and as they were when recording stopped.
    heights: [f32; BARS],
    frozen: Option<[f32; BARS]>,
    started: Instant,
    levels: [f32; BANDS],
    partial: String,
    waiting: bool,
}

/// A window's voice typing.
pub(crate) struct Voice {
    trigger: Trigger,
    epoch: Instant,
    live: Option<Live>,
    ticker: Option<Task<()>>,
    _activation: Option<gpui::Subscription>,
}

/// The pill fades out over this.
const LEAVE: Duration = Duration::from_millis(240);

impl Voice {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.voice_input(Input::Blur, window, cx);
            }
        });
        Self { trigger: Trigger::default(), epoch: Instant::now(), live: None, ticker: None, _activation: Some(activation) }
    }

    /// Recording or finishing: Esc belongs to it.
    fn active(&self) -> bool {
        self.live.as_ref().is_some_and(|l| !matches!(l.stage, Stage::Leaving(_)))
    }
}

impl Workspace {
    /// Fn and the other modifiers, from the window's root.
    pub(crate) fn voice_modifiers(&mut self, event: &ModifiersChangedEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.modifiers;
        self.voice_input(Input::Modifiers { function: m.function, others: m.control || m.alt || m.shift || m.platform }, window, cx);
    }

    /// Every key down, before the focused block sees it; Esc stops there
    /// while dictating.
    pub(crate) fn voice_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let escape = event.keystroke.key == "escape";
        let active = self.voice.active();
        self.voice_input(Input::Key { escape }, window, cx);
        if escape && active {
            if let Some(live) = self.voice.live.as_mut().filter(|l| l.stage != Stage::Recording) {
                // Esc while transcribing drops the text too.
                live.session.cancel();
                live.stage = Stage::Leaving(Instant::now());
                self.voice.trigger.reset();
            }
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn voice_input(&mut self, input: Input, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.voice.on && !self.voice.trigger.recording() {
            return;
        }
        let now = self.voice.epoch.elapsed();
        match self.voice.trigger.input(input, now) {
            Some(Action::StartHold) => self.voice_start(false, window, cx),
            Some(Action::StartHandsFree) => self.voice_start(true, window, cx),
            Some(Action::Stop) => self.voice_stop(false, cx),
            Some(Action::Cancel) => self.voice_stop(true, cx),
            None => {}
        }
        if self.voice.trigger.wants_ticks() && self.voice.ticker.is_none() {
            self.voice.ticker = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(20)).await;
                    let more = this.update_in(cx, |this, window, cx| {
                        this.voice_input(Input::Tick, window, cx);
                        this.voice.trigger.wants_ticks()
                    });
                    if !more.unwrap_or(false) {
                        break;
                    }
                }
                let _ = this.update(cx, |this, _| this.voice.ticker = None);
            }));
        }
    }

    fn voice_start(&mut self, hands_free: bool, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.state.voice.clone();
        let engine = prefs.engine;
        if self.voice.active() {
            // Still transcribing the last one.
            self.voice.trigger.reset();
            return;
        }
        if !silo_voice::engine::available(engine) {
            self.voice.trigger.reset();
            self.flash(Status::error(format!("{} voice typing isn't available on this Mac", engine.label())), cx);
            return;
        }
        #[cfg(target_os = "macos")]
        if silo_voice::permission::access() == silo_voice::permission::Access::Denied {
            self.voice.trigger.reset();
            self.flash(Status::warning("Silo can't use the microphone; allow it in Settings › Permissions"), cx);
            return;
        }
        // The first use asks; recording starts with the next press once allowed.
        #[cfg(target_os = "macos")]
        if silo_voice::permission::access() == silo_voice::permission::Access::NotAsked {
            self.voice.trigger.reset();
            silo_voice::permission::request(|_| {});
            return;
        }
        let model = engine.model();
        fetch::start(self.home.clone(), engine, &self.rt);
        let options = Options { language: engine.language(&prefs.language), keywords: self.voice_hints(window, cx) };
        let session =
            Session::start(Config { engine, model: model.path(&self.home), model_bytes: model.bytes, options, microphone: prefs.microphone.clone() });
        self.voice.live = Some(Live {
            session,
            hands_free,
            engine,
            stage: Stage::Recording,
            heights: [0.0; BARS],
            frozen: None,
            started: Instant::now(),
            levels: [0.0; BANDS],
            partial: String::new(),
            waiting: false,
        });
        if prefs.sounds {
            sound::play(sound::Sound::ListenStart);
        }
        cx.notify();
    }

    fn voice_stop(&mut self, cancel: bool, cx: &mut Context<Self>) {
        let Some(live) = self.voice.live.as_mut().filter(|l| l.stage == Stage::Recording) else { return };
        let now = Instant::now();
        if cancel {
            live.session.cancel();
            live.stage = Stage::Leaving(now);
        } else {
            live.session.stop();
            live.stage = Stage::Finishing(now);
        }
        if self.state.voice.sounds {
            sound::play(sound::Sound::ListenStop);
        }
        cx.notify();
    }

    /// Follow the dictation each frame: levels, live words, the result.
    fn voice_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = self.voice.live.as_mut() else { return };
        let snapshot = live.session.snapshot();
        let now = Instant::now();
        live.levels = snapshot.levels;
        live.partial = snapshot.partial;
        live.waiting = snapshot.phase == Phase::WaitingForModel;
        if let Stage::Leaving(since) = live.stage {
            if now.duration_since(since) > LEAVE {
                self.voice.live = None;
            }
            return;
        }
        match snapshot.phase {
            Phase::Recording => {}
            // It stopped by itself (the length limit).
            Phase::WaitingForModel | Phase::Transcribing if live.stage == Stage::Recording => {
                live.stage = Stage::Finishing(now);
                self.voice.trigger.reset();
            }
            Phase::WaitingForModel | Phase::Transcribing => {}
            Phase::Done(text) => {
                live.stage = Stage::Leaving(now);
                self.voice.trigger.reset();
                if !text.is_empty() {
                    self.voice_insert(text, window, cx);
                }
            }
            Phase::Failed(error) => {
                live.stage = Stage::Leaving(now);
                self.voice.trigger.reset();
                self.flash(Status::error(error), cx);
            }
        }
    }
}
