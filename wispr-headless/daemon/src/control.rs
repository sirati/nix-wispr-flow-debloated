//! Who dictates, and who has the flow bar. One dictation records at a time,
//! for Wispr or for the local model, whether it was started by our shortcut
//! (hotkey/), by a command, or by Wispr itself; and the one bar shows the
//! newest dictation. Pure state: serve() in main.rs feeds it what happens
//! and carries out what it asks for, so every sequence can be tested here.
//!
//! A press while a dictation records never starts a second one:
//!
//! - A press that the shortcut reads as the end (a hold's release, any
//!   binding's press while hands-free) ends it. So does a hold binding going
//!   down again while one records (its release was missed): the dictation
//!   ends, and that press starts nothing.
//! - A press while Wispr's own dictation records (one Wispr started itself)
//!   starts nothing; if that dictation is hands-free, the press stops it, as
//!   Wispr's own shortcut would.
//! - A dictation that is only being transcribed records nothing: a press
//!   then starts the next one. For the local model the next waits for the
//!   model and takes it over (asr/mod.rs, Slot); the bar shows the new one,
//!   and the earlier text is still typed when it is ready.
//!
//! Every recording has an end: its shortcut, Escape, a click on the bar, the
//! time limit (`--max-dictation-seconds`), the microphone or the model
//! failing, or Wispr taking over. Wispr's own dictations Wispr ends itself.
//!
//! Wispr is driven through its own shortcuts: the shim injects presses of
//! Wispr's push-to-talk and hands-free keys (F20 and F19 by default) into
//! the key events Wispr's helper sends, so Wispr runs its usual flow and
//! shows its usual bar.

use std::time::{Duration, Instant};

use crate::protocol::Backend;

/// Wispr's shortcut actions the shim can press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WisprKey {
    /// Push-to-talk ("ptt").
    Ptt,
    /// Hands-free ("popo").
    Popo,
}

impl WisprKey {
    pub fn name(self) -> &'static str {
        match self {
            WisprKey::Ptt => "ptt",
            WisprKey::Popo => "popo",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A hold binding went down: perhaps a dictation, so record silently.
    Down(Backend),
    /// A dictation starts; `hands_free`: it goes on without the key.
    Start { backend: Backend, hands_free: bool },
    /// The dictation goes on without the key from now on.
    HandsFree,
    /// Stop and transcribe.
    End,
    /// Not a dictation after all, or Escape: drop it.
    Cancel,
    /// Wispr's `status:dictationStatus`.
    WisprStatus(String),
    /// `status:popo`: Wispr's dictation is hands-free.
    WisprPopo,
    /// Wispr asks to record (`dictation:audioStart`).
    WisprStart,
    /// The local dictation's microphone or model failed while it recorded.
    LocalFailed(String),
    /// A local dictation's transcription is done.
    Transcribed { ok: bool },
    /// The flow bar's button: start, stop or cancel.
    BarClick(String),
    /// The Undo of a cancelled local dictation's notice: transcribe it after
    /// all.
    Undo,
    /// Time passed.
    Tick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Record without showing anything, to keep what is said before the
    /// dictation is certain (or before Wispr asks).
    StartEarly,
    /// Drop that recording.
    DropEarly,
    /// Start a local dictation with the early recording, if there is one.
    StartLocal { hands_free: bool },
    /// Stop recording; transcribe the rest (then Input::Transcribed).
    EndLocal,
    /// Stop the local dictation untranscribed, keeping it for a while for
    /// the bar's Undo.
    CancelLocal,
    /// Transcribe the cancelled local dictation after all (then
    /// Input::Transcribed).
    UndoLocal,
    /// Another dictation starts: forget the cancelled one and its Undo.
    DropCancelled,
    /// Drop it and report the error.
    FailLocal(String),
    /// Ask the shim to press (true) or release Wispr's shortcut key.
    WisprKey(WisprKey, bool),
    /// A click on Wispr's bar button: start, stop, cancel.
    WisprClick(&'static str),
    /// Answer Wispr's request to record with a failure: the microphone is
    /// taken.
    RefuseWisprStart,
    /// The flow bar's dictation status (bar.rs).
    BarStatus(&'static str),
    /// The bar shows a hands-free dictation.
    BarHandsFree,
    /// The shortcut stops waiting for the keys that would end the
    /// dictation, which ended otherwise.
    ResetShortcut,
    Log(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// A hold binding is down, not yet a dictation; recording silently.
    Armed,
    /// A local dictation records.
    Local { hands_free: bool, since: Instant },
    /// Wispr's dictation, started with its shortcut keys by us.
    Wispr { hands_free: bool },
    /// The rest of a gesture that started nothing: its end ends nothing.
    Swallow,
}

pub struct Control {
    phase: Phase,
    /// Wispr's push-to-talk key is down as far as Wispr knows.
    ptt_down: bool,
    /// Wispr's dictation records (initializing, listening).
    wispr_recording: bool,
    wispr_hands_free: bool,
    /// Local dictations being transcribed.
    finishing: usize,
    /// The bar shows a local dictation; Wispr's status does not reach it.
    local_bar: bool,
    max: Duration,
}

impl Control {
    /// `max`: the longest a local dictation records.
    pub fn new(max: Duration) -> Control {
        Control {
            phase: Phase::Idle,
            ptt_down: false,
            wispr_recording: false,
            wispr_hands_free: false,
            finishing: 0,
            local_bar: false,
            max,
        }
    }

    /// Whether Wispr's status messages may drive the bar.
    pub fn wispr_owns_bar(&self) -> bool {
        !self.local_bar
    }

    /// Whether a local dictation records.
    pub fn local_recording(&self) -> bool {
        matches!(self.phase, Phase::Local { .. })
    }

    /// Whether anything of ours records or may soon: no early recording
    /// for Wispr's own dictation then.
    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// For the log.
    pub fn describe(&self) -> String {
        let phase = match self.phase {
            Phase::Idle => "idle".to_string(),
            Phase::Armed => "armed".to_string(),
            Phase::Local { hands_free, .. } => {
                format!("local{}", if hands_free { " hands-free" } else { "" })
            }
            Phase::Wispr { hands_free } => {
                format!("wispr{}", if hands_free { " hands-free" } else { "" })
            }
            Phase::Swallow => "ignoring the rest of the press".to_string(),
        };
        format!(
            "{phase}; wispr {}; {} transcribing; bar {}",
            if self.wispr_recording {
                "recording"
            } else {
                "not recording"
            },
            self.finishing,
            if self.local_bar { "local" } else { "wispr" },
        )
    }

    /// Nothing records, so a dictation may start.
    fn free(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Armed) && !self.wispr_recording
    }

    pub fn input(&mut self, input: Input, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        match input {
            Input::Down(_) => match self.phase {
                Phase::Idle if self.free() => {
                    self.phase = Phase::Armed;
                    out.push(Action::StartEarly);
                }
                Phase::Idle => {
                    if self.wispr_recording && self.wispr_hands_free {
                        out.push(Action::Log("a press stops Wispr's hands-free dictation".into()));
                        self.press_popo(&mut out);
                    } else {
                        out.push(Action::Log("a press while Wispr records: ignored".into()));
                    }
                    self.phase = Phase::Swallow;
                }
                Phase::Local { .. } | Phase::Wispr { .. } => {
                    out.push(Action::Log(
                        "a press while dictating (its release was missed): ending it".into(),
                    ));
                    self.end(&mut out);
                    self.phase = Phase::Swallow;
                }
                Phase::Armed | Phase::Swallow => {}
            },
            Input::Start {
                backend,
                hands_free,
            } => match self.phase {
                Phase::Idle | Phase::Armed if self.free() => {
                    let early = self.phase == Phase::Armed;
                    self.start(backend, hands_free, early, now, &mut out);
                }
                Phase::Idle | Phase::Armed => {
                    if self.phase == Phase::Armed {
                        out.push(Action::DropEarly);
                    }
                    out.push(Action::Log(
                        "no dictation: Wispr's dictation is recording".into(),
                    ));
                    self.phase = Phase::Swallow;
                }
                Phase::Local { .. } | Phase::Wispr { .. } => {
                    out.push(Action::Log(
                        "a start while dictating: ending the dictation instead".into(),
                    ));
                    self.end(&mut out);
                    self.phase = Phase::Swallow;
                }
                Phase::Swallow => {}
            },
            Input::HandsFree => match self.phase {
                Phase::Local {
                    hands_free: false,
                    since,
                } => {
                    self.phase = Phase::Local {
                        hands_free: true,
                        since,
                    };
                    out.push(Action::BarHandsFree);
                }
                Phase::Wispr { hands_free: false } => {
                    // Wispr's hands-free key turns its push-to-talk
                    // dictation hands-free; the push-to-talk key's release
                    // then changes nothing.
                    self.press_popo(&mut out);
                    self.release_ptt(&mut out);
                    self.phase = Phase::Wispr { hands_free: true };
                }
                _ => {}
            },
            Input::End => {
                self.end(&mut out);
                self.phase = Phase::Idle;
            }
            Input::Cancel => {
                self.cancel(&mut out);
                self.phase = Phase::Idle;
            }
            Input::WisprStatus(value) => match value.as_str() {
                "initializing" => {
                    if self.local_recording() {
                        // Wispr started one itself (its bar, its own
                        // shortcut): the microphone is taken.
                        out.push(Action::Log(
                            "Wispr started a dictation while a local one records: cancelling Wispr's"
                                .into(),
                        ));
                        out.push(Action::WisprClick("cancel"));
                    } else {
                        self.local_bar = false;
                        out.push(Action::DropCancelled);
                    }
                    self.wispr_recording = true;
                    self.wispr_hands_free = false;
                }
                "listening" => self.wispr_recording = true,
                "stopping" | "processing" | "retrying" => self.wispr_recording = false,
                "idle" | "dismissed" | "error" => {
                    self.wispr_recording = false;
                    self.wispr_hands_free = false;
                    if let Phase::Wispr { hands_free: true } = self.phase {
                        // Wispr ended it (Escape, its limit, an error).
                        self.phase = Phase::Idle;
                        out.push(Action::ResetShortcut);
                    }
                }
                _ => {}
            },
            Input::WisprPopo => self.wispr_hands_free = true,
            Input::WisprStart => {
                if self.local_recording() {
                    out.push(Action::RefuseWisprStart);
                }
            }
            Input::LocalFailed(error) => {
                if self.local_recording() {
                    self.phase = Phase::Idle;
                    out.push(Action::Log(format!("the local dictation failed: {error}")));
                    out.push(Action::FailLocal(error));
                    out.push(Action::ResetShortcut);
                    if self.local_bar {
                        out.push(Action::BarStatus("error"));
                        if self.finishing == 0 {
                            self.local_bar = false;
                        }
                    }
                }
            }
            Input::Transcribed { ok } => {
                self.finishing = self.finishing.saturating_sub(1);
                // The bar belongs to the newest dictation.
                if self.local_bar && !self.local_recording() && self.finishing == 0 {
                    self.local_bar = false;
                    out.push(Action::BarStatus(if ok { "idle" } else { "error" }));
                }
            }
            Input::BarClick(action) => {
                if self.local_bar {
                    if self.local_recording() {
                        match action.as_str() {
                            "stop" => {
                                self.end(&mut out);
                                self.phase = Phase::Idle;
                                out.push(Action::ResetShortcut);
                            }
                            "cancel" => {
                                self.cancel(&mut out);
                                self.phase = Phase::Idle;
                                out.push(Action::ResetShortcut);
                            }
                            _ => {}
                        }
                    }
                } else {
                    match action.as_str() {
                        // Wispr's resting bar starts Wispr's hands-free
                        // dictation, unless something records.
                        "start" if self.phase == Phase::Idle && self.free() => {
                            out.push(Action::WisprClick("start"))
                        }
                        "start" => {}
                        "stop" => out.push(Action::WisprClick("stop")),
                        "cancel" => out.push(Action::WisprClick("cancel")),
                        _ => {}
                    }
                }
            }
            Input::Undo => {
                self.finishing += 1;
                out.push(Action::UndoLocal);
                if self.phase == Phase::Idle && !self.wispr_recording {
                    self.local_bar = true;
                    out.push(Action::BarStatus("processing"));
                }
            }
            Input::Tick => {
                if let Phase::Local { since, .. } = self.phase {
                    if now.duration_since(since) >= self.max {
                        out.push(Action::Log(format!(
                            "the local dictation reached {} s: ending it",
                            self.max.as_secs()
                        )));
                        self.end(&mut out);
                        self.phase = Phase::Idle;
                        out.push(Action::ResetShortcut);
                    }
                }
            }
        }
        out
    }

    fn start(
        &mut self,
        backend: Backend,
        hands_free: bool,
        early: bool,
        now: Instant,
        out: &mut Vec<Action>,
    ) {
        out.push(Action::DropCancelled);
        match backend {
            Backend::Local => {
                self.phase = Phase::Local {
                    hands_free,
                    since: now,
                };
                self.local_bar = true;
                out.push(Action::StartLocal { hands_free });
            }
            Backend::Wispr => {
                // Recording from now, for Wispr to adopt when it asks.
                if !early {
                    out.push(Action::StartEarly);
                }
                if hands_free {
                    self.press_popo(out);
                } else {
                    out.push(Action::WisprKey(WisprKey::Ptt, true));
                    self.ptt_down = true;
                }
                self.phase = Phase::Wispr { hands_free };
            }
        }
    }

    fn press_popo(&mut self, out: &mut Vec<Action>) {
        out.push(Action::WisprKey(WisprKey::Popo, true));
        out.push(Action::WisprKey(WisprKey::Popo, false));
    }

    fn release_ptt(&mut self, out: &mut Vec<Action>) {
        if std::mem::take(&mut self.ptt_down) {
            out.push(Action::WisprKey(WisprKey::Ptt, false));
        }
    }

    /// Ends what records (the phase is set by the caller).
    fn end(&mut self, out: &mut Vec<Action>) {
        match self.phase {
            Phase::Armed => out.push(Action::DropEarly),
            Phase::Local { .. } => {
                self.finishing += 1;
                out.push(Action::EndLocal);
                out.push(Action::BarStatus("processing"));
            }
            Phase::Wispr { hands_free: true } => {
                // Wispr's hands-free key again: stop (or, within 500 ms of
                // its start, cancel, as in Wispr).
                self.press_popo(out);
            }
            Phase::Wispr { hands_free: false } => {}
            Phase::Idle | Phase::Swallow => {}
        }
        self.release_ptt(out);
    }

    /// Drops what records (the phase is set by the caller).
    fn cancel(&mut self, out: &mut Vec<Action>) {
        match self.phase {
            Phase::Armed => out.push(Action::DropEarly),
            Phase::Local { .. } => {
                out.push(Action::CancelLocal);
                out.push(Action::BarStatus(if self.finishing > 0 {
                    "processing"
                } else {
                    "idle"
                }));
                if self.finishing == 0 {
                    self.local_bar = false;
                }
            }
            Phase::Wispr { .. } => {
                // Cancel first: the push-to-talk key's release alone would
                // submit what was said.
                out.push(Action::WisprClick("cancel"));
                out.push(Action::DropEarly);
            }
            Phase::Idle | Phase::Swallow => {}
        }
        self.release_ptt(out);
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::*;
    use crate::hotkey::{Binding, Chord, Hotkey, Key, Mode, ARM, DOUBLE_PRESS};

    const OPTION: Key = 61;
    const COMMAND: Key = 54;
    const ESC: Key = 53;
    const C: Key = 8;

    /// The chord and the controller as serve() wires them, with stand-ins
    /// for the recorder, the local model and Wispr, which check the rules:
    /// one recording at a time, every recording ends, Wispr's keys pair up.
    struct Sim {
        chord: Chord,
        ctl: Control,
        t0: Instant,
        now: u64,
        log: Vec<String>,
        early: bool,
        local: bool,
        /// A cancelled local dictation waits for its Undo.
        cancelled: bool,
        /// Local dictations being transcribed (the model's threads).
        finishing: usize,
        ptt: bool,
        /// Wispr's own state, as its keyboard service would have it.
        wispr: WisprSim,
        bar: Vec<&'static str>,
        /// Statuses Wispr sent while a local dictation had the bar.
        clicks: Vec<&'static str>,
        refused: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum WisprSim {
        Idle,
        Ptt,
        Locked,
    }

    impl Sim {
        fn new() -> Sim {
            let bindings = vec![
                Binding {
                    keys: vec![OPTION],
                    mode: Mode::Hold,
                    backend: Some("wispr".into()),
                },
                Binding {
                    keys: vec![COMMAND],
                    mode: Mode::Hold,
                    backend: Some("local".into()),
                },
            ];
            Sim {
                chord: Chord::new(bindings, ARM)
                    .double_press(DOUBLE_PRESS)
                    .escape(Some(ESC)),
                ctl: Control::new(Duration::from_secs(600)),
                t0: Instant::now(),
                now: 0,
                log: Vec::new(),
                early: false,
                local: false,
                cancelled: false,
                finishing: 0,
                ptt: false,
                wispr: WisprSim::Idle,
                bar: Vec::new(),
                clicks: Vec::new(),
                refused: 0,
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        fn backend(&self) -> Backend {
            match self.chord.owner().and_then(|b| b.backend.as_deref()) {
                Some("wispr") => Backend::Wispr,
                _ => Backend::Local,
            }
        }

        /// What serve() makes of a batch of shortcut events.
        fn hotkey(&mut self, events: Vec<Hotkey>) {
            let backend = self.backend();
            let mut i = 0;
            while i < events.len() {
                let input = match events[i] {
                    Hotkey::Down => Input::Down(backend),
                    Hotkey::Pressed => {
                        let hands_free = events.get(i + 1) == Some(&Hotkey::HandsFree);
                        i += usize::from(hands_free);
                        Input::Start {
                            backend,
                            hands_free,
                        }
                    }
                    Hotkey::HandsFree => Input::HandsFree,
                    Hotkey::Released => Input::End,
                    Hotkey::Cancelled => Input::Cancel,
                };
                self.feed(input);
                i += 1;
            }
        }

        fn advance(&mut self, ms: u64) {
            while let Some(d) = self.chord.deadline() {
                if d > self.at(ms) {
                    break;
                }
                let t = d.duration_since(self.t0).as_millis() as u64;
                self.now = self.now.max(t);
                let events = self.chord.tick(d);
                self.hotkey(events);
            }
            self.now = self.now.max(ms);
            self.feed(Input::Tick);
        }

        fn key(&mut self, ms: u64, key: Key, down: bool) {
            self.advance(ms);
            let events = self.chord.key(key, down, self.at(ms));
            self.hotkey(events);
        }

        fn down(&mut self, ms: u64, key: Key) {
            self.key(ms, key, true);
        }

        fn up(&mut self, ms: u64, key: Key) {
            self.key(ms, key, false);
        }

        fn tap(&mut self, ms: u64, key: Key) {
            self.down(ms, key);
            self.up(ms + 80, key);
        }

        fn feed(&mut self, input: Input) {
            let now = self.at(self.now);
            let actions = self.ctl.input(input, now);
            for a in actions {
                self.act(a);
            }
        }

        fn act(&mut self, a: Action) {
            match a {
                StartEarly => {
                    if !self.local {
                        self.early = true;
                    }
                }
                DropEarly => self.early = false,
                StartLocal { .. } => {
                    assert!(!self.local, "two local recordings at {}", self.now);
                    assert!(
                        self.wispr == WisprSim::Idle,
                        "a local dictation while Wispr records at {}",
                        self.now
                    );
                    self.early = false;
                    self.local = true;
                    self.bar.push("local");
                }
                EndLocal => {
                    assert!(self.local);
                    self.local = false;
                    self.finishing += 1;
                }
                CancelLocal => {
                    assert!(self.local);
                    self.local = false;
                    self.cancelled = true;
                }
                FailLocal(_) => {
                    assert!(self.local);
                    self.local = false;
                }
                UndoLocal => {
                    assert!(std::mem::take(&mut self.cancelled));
                    self.finishing += 1;
                }
                DropCancelled => self.cancelled = false,
                WisprKey(key, down) => self.wispr_key(key, down),
                WisprClick(c) => {
                    self.clicks.push(c);
                    if c == "cancel" && self.wispr != WisprSim::Idle {
                        self.wispr_status("idle");
                    }
                }
                RefuseWisprStart => self.refused += 1,
                BarStatus(s) => self.bar.push(s),
                BarHandsFree => self.bar.push("handsFree"),
                ResetShortcut => {
                    let events = self.chord.key(crate::hotkey::RESET, true, self.at(self.now));
                    assert!(events.is_empty());
                }
                Log(m) => self.log.push(m),
            }
        }

        /// Wispr's keyboard service, roughly: push-to-talk starts on its
        /// press and stops on its release; hands-free starts locked and its
        /// next press stops.
        fn wispr_key(&mut self, key: super::WisprKey, down: bool) {
            match (key, down) {
                (super::WisprKey::Ptt, true) => {
                    assert!(!self.ptt, "push-to-talk pressed twice");
                    self.ptt = true;
                    if self.wispr == WisprSim::Idle {
                        self.wispr = WisprSim::Ptt;
                        self.wispr_begin();
                    }
                }
                (super::WisprKey::Ptt, false) => {
                    assert!(self.ptt, "push-to-talk released while up");
                    self.ptt = false;
                    if self.wispr == WisprSim::Ptt {
                        self.wispr_status("idle");
                    }
                }
                (super::WisprKey::Popo, true) => match self.wispr {
                    WisprSim::Idle => {
                        self.wispr = WisprSim::Locked;
                        self.wispr_begin();
                        self.feed(Input::WisprPopo);
                    }
                    WisprSim::Ptt => {
                        self.wispr = WisprSim::Locked;
                        self.feed(Input::WisprPopo);
                    }
                    WisprSim::Locked => self.wispr_status("idle"),
                },
                (super::WisprKey::Popo, false) => {}
            }
        }

        fn wispr_begin(&mut self) {
            self.wispr_status("initializing");
            let refused = self.refused;
            self.feed(Input::WisprStart);
            if self.refused > refused {
                self.wispr_status("idle");
                return;
            }
            assert!(!self.local, "Wispr records while a local dictation does");
            self.early = false;
            self.wispr_status("listening");
        }

        fn wispr_status(&mut self, s: &str) {
            if s == "idle" {
                self.wispr = WisprSim::Idle;
            }
            self.feed(Input::WisprStatus(s.into()));
            if self.ctl.wispr_owns_bar() {
                self.bar.push(match s {
                    "initializing" => "wispr",
                    "listening" => "listening",
                    _ => "idle",
                });
            }
        }

        /// The model finishes the oldest transcription.
        fn transcribed(&mut self, ok: bool) {
            assert!(self.finishing > 0);
            self.finishing -= 1;
            self.feed(Input::Transcribed { ok });
        }

        fn recording(&self) -> bool {
            self.local || self.wispr != WisprSim::Idle
        }

        /// Everything let go and waited for: nothing records, nothing is
        /// held down in Wispr, and the bar is not left on a local dictation.
        fn settle(&mut self, ms: u64) {
            for k in [OPTION, COMMAND, ESC, C] {
                self.up(ms, k);
            }
            self.advance(ms + 5_000);
            while self.finishing > 0 {
                self.transcribed(true);
            }
            assert!(!self.early, "an early recording was left");
            assert!(!self.ptt, "Wispr's push-to-talk key was left down");
        }
    }

    #[test]
    fn hold_command_is_a_local_push_to_talk_dictation() {
        let mut s = Sim::new();
        s.down(0, COMMAND);
        assert!(s.early && !s.local, "recording silently, nothing shown");
        assert!(s.bar.is_empty());
        s.advance(200);
        assert!(s.local);
        assert_eq!(s.bar, ["local"]);
        s.up(3000, COMMAND);
        assert!(!s.local);
        assert_eq!(s.finishing, 1);
        s.transcribed(true);
        assert_eq!(s.bar, ["local", "processing", "idle"]);
        s.settle(4000);
    }

    #[test]
    fn a_tap_starts_nothing_and_leaves_nothing() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        assert!(s.early, "kept for a possible second press");
        s.advance(1000);
        assert!(!s.early && !s.local && s.bar.is_empty());
        s.tap(2000, OPTION);
        s.advance(3000);
        assert!(!s.early && s.wispr == WisprSim::Idle && s.bar.is_empty());
        s.settle(4000);
    }

    #[test]
    fn a_double_press_on_command_is_a_local_hands_free_dictation() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(250, COMMAND);
        assert!(s.local);
        assert_eq!(s.bar, ["local"]);
        s.up(330, COMMAND);
        s.advance(10_000);
        assert!(s.local, "goes on without the key");
        // Either key ends it; that press starts nothing.
        s.down(20_000, OPTION);
        assert!(!s.local && s.wispr == WisprSim::Idle);
        s.advance(21_000);
        s.up(21_500, OPTION);
        assert!(s.wispr == WisprSim::Idle);
        s.settle(22_000);
    }

    #[test]
    fn hold_option_is_wispr_s_push_to_talk() {
        let mut s = Sim::new();
        s.down(0, OPTION);
        assert!(s.early && !s.ptt);
        s.advance(200);
        assert!(s.ptt && s.wispr == WisprSim::Ptt);
        assert!(!s.early, "Wispr adopted the early recording");
        s.up(2000, OPTION);
        assert!(!s.ptt && s.wispr == WisprSim::Idle);
        s.settle(3000);
    }

    #[test]
    fn a_double_press_on_option_is_wispr_s_hands_free() {
        let mut s = Sim::new();
        s.tap(0, OPTION);
        s.down(200, OPTION);
        assert_eq!(s.wispr, WisprSim::Locked);
        assert!(!s.ptt, "hands-free goes through Wispr's own hands-free key");
        s.up(260, OPTION);
        s.advance(5_000);
        assert_eq!(s.wispr, WisprSim::Locked);
        // Command stops it, and starts no local dictation.
        s.down(6_000, COMMAND);
        assert_eq!(s.wispr, WisprSim::Idle);
        s.advance(7_000);
        assert!(!s.local);
        s.up(7_100, COMMAND);
        s.settle(8_000);
    }

    #[test]
    fn hold_option_while_a_local_hands_free_dictation_runs_only_ends_it() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(260, COMMAND);
        assert!(s.local);
        s.down(3_000, OPTION);
        assert!(!s.local);
        s.advance(5_000);
        assert!(!s.ptt && s.wispr == WisprSim::Idle, "Wispr not started");
        s.up(5_100, OPTION);
        s.settle(6_000);
    }

    #[test]
    fn a_double_press_on_command_while_wispr_holds_cancels_wispr_only() {
        let mut s = Sim::new();
        s.down(0, OPTION);
        s.advance(300);
        assert_eq!(s.wispr, WisprSim::Ptt);
        // Another key while holding: not a dictation after all.
        s.tap(1_000, COMMAND);
        s.tap(1_200, COMMAND);
        assert_eq!(s.wispr, WisprSim::Idle);
        assert_eq!(s.clicks, ["cancel"]);
        assert!(!s.local, "nothing starts while Option is still down");
        s.up(1_500, OPTION);
        s.advance(3_000);
        assert!(!s.local && !s.ptt);
        s.settle(4_000);
    }

    #[test]
    fn a_double_press_on_command_while_wispr_is_hands_free_stops_wispr() {
        let mut s = Sim::new();
        s.tap(0, OPTION);
        s.down(150, OPTION);
        s.up(200, OPTION);
        assert_eq!(s.wispr, WisprSim::Locked);
        s.tap(2_000, COMMAND);
        assert_eq!(s.wispr, WisprSim::Idle);
        // The second press is a press of its own: a tap, nothing.
        s.tap(2_200, COMMAND);
        s.advance(4_000);
        assert!(!s.local);
        s.settle(5_000);
    }

    #[test]
    fn a_press_while_finishing_starts_the_next_local_dictation() {
        let mut s = Sim::new();
        s.down(0, COMMAND);
        s.up(1_000, COMMAND);
        assert_eq!(s.finishing, 1);
        s.down(1_100, COMMAND);
        s.advance(1_400);
        assert!(s.local, "the next one records while the first is transcribed");
        // The first one's text does not take the bar from the second.
        s.transcribed(true);
        assert_eq!(s.bar.last(), Some(&"local"));
        s.up(3_000, COMMAND);
        s.transcribed(true);
        assert_eq!(s.bar.last(), Some(&"idle"));
        s.settle(4_000);
    }

    #[test]
    fn escape_cancels_a_local_hands_free_dictation() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        assert!(s.local);
        s.down(1_000, ESC);
        assert!(!s.local);
        assert_eq!(s.finishing, 0, "not transcribed");
        assert_eq!(s.bar.last(), Some(&"idle"));
        s.up(1_050, ESC);
        s.settle(2_000);
    }

    #[test]
    fn undo_transcribes_a_cancelled_local_dictation() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        s.down(1_000, ESC);
        s.up(1_050, ESC);
        assert!(s.cancelled && s.finishing == 0);
        s.feed(Input::Undo);
        assert!(!s.cancelled && s.finishing == 1);
        assert_eq!(s.bar.last(), Some(&"processing"));
        s.transcribed(true);
        assert_eq!(s.bar.last(), Some(&"idle"));
        s.settle(2_000);
    }

    #[test]
    fn a_new_dictation_forgets_the_cancelled_one() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        s.down(1_000, ESC);
        s.up(1_050, ESC);
        assert!(s.cancelled);
        s.down(2_000, OPTION);
        s.advance(2_500);
        assert!(!s.cancelled);
        s.up(3_000, OPTION);
        s.settle(4_000);
    }

    #[test]
    fn a_local_dictation_ends_itself_at_the_time_limit() {
        let mut s = Sim::new();
        s.ctl.max = Duration::from_secs(10);
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        s.advance(10_100);
        assert!(s.local, "10 s from its start at 200 ms");
        s.advance(10_200);
        assert!(!s.local && s.finishing == 1);
        assert!(s.log.iter().any(|l| l.contains("reached 10 s")));
        // The shortcut was reset: the next press is a new one.
        s.down(12_000, COMMAND);
        s.advance(12_300);
        assert!(s.local);
        s.up(13_000, COMMAND);
        s.settle(14_000);
    }

    #[test]
    fn a_failing_model_ends_the_dictation_with_an_error() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        s.advance(3_000);
        s.feed(Input::LocalFailed("llama-server ended (signal: 9)".into()));
        assert!(!s.local);
        assert_eq!(s.bar.last(), Some(&"error"));
        // The next press starts afresh.
        s.down(5_000, COMMAND);
        s.advance(5_300);
        assert!(s.local);
        s.up(6_000, COMMAND);
        s.settle(7_000);
    }

    #[test]
    fn a_missed_release_is_ended_by_the_next_press() {
        let mut s = Sim::new();
        s.down(0, COMMAND);
        s.advance(300);
        assert!(s.local);
        // The keyboard layer resynchronises (hotkey/mac.rs): up, then down.
        s.up(5_000, COMMAND);
        s.down(5_000, COMMAND);
        assert!(!s.local);
        // Even with the chord out of step, a Down while dictating ends it.
        s.feed(Input::Start {
            backend: Backend::Local,
            hands_free: true,
        });
        assert!(s.local);
        s.feed(Input::Down(Backend::Local));
        assert!(!s.local);
        s.feed(Input::Start {
            backend: Backend::Local,
            hands_free: false,
        });
        assert!(!s.local, "the rest of that press starts nothing");
        s.feed(Input::End);
        s.settle(6_000);
    }

    #[test]
    fn wispr_s_own_dictation_blocks_ours_and_is_blocked_by_ours() {
        let mut s = Sim::new();
        // Wispr starts one itself (its bar).
        s.wispr = WisprSim::Locked;
        s.wispr_begin();
        s.feed(Input::WisprPopo);
        // A hold on Command: nothing; and since Wispr's is hands-free, the
        // press stops it.
        s.down(0, COMMAND);
        assert_eq!(s.wispr, WisprSim::Idle);
        s.advance(400);
        assert!(!s.local);
        s.up(500, COMMAND);
        // A push-to-talk one of Wispr's own (not hands-free): ignored.
        s.wispr = WisprSim::Ptt;
        s.wispr_begin();
        s.down(1_000, COMMAND);
        s.advance(1_400);
        assert!(!s.local && s.wispr == WisprSim::Ptt);
        s.up(1_500, COMMAND);
        s.wispr_status("idle");
        // And the other way round: Wispr's own start while a local one
        // records is cancelled and refused the microphone.
        s.down(2_000, COMMAND);
        s.advance(2_300);
        assert!(s.local);
        s.wispr = WisprSim::Locked;
        s.wispr_begin();
        assert_eq!(s.refused, 1);
        assert!(s.clicks.contains(&"cancel"));
        assert!(s.local, "the local dictation goes on");
        assert_eq!(s.bar.last(), Some(&"local"), "Wispr's status did not take the bar");
        s.up(3_000, COMMAND);
        s.settle(4_000);
    }

    #[test]
    fn the_bar_s_buttons_end_a_local_dictation() {
        let mut s = Sim::new();
        s.tap(0, COMMAND);
        s.down(200, COMMAND);
        s.up(250, COMMAND);
        s.feed(Input::BarClick("stop".into()));
        assert!(!s.local && s.finishing == 1);
        assert!(s.clicks.is_empty(), "Wispr was not told");
        s.transcribed(true);
        s.tap(2_000, COMMAND);
        s.down(2_200, COMMAND);
        s.up(2_250, COMMAND);
        s.feed(Input::BarClick("cancel".into()));
        assert!(!s.local && s.finishing == 0);
        // A start click while a local one records is dropped.
        s.down(3_000, COMMAND);
        s.advance(3_300);
        s.feed(Input::BarClick("start".into()));
        assert!(s.clicks.is_empty());
        s.up(4_000, COMMAND);
        s.settle(5_000);
    }

    #[test]
    fn a_local_press_during_the_model_load_and_spam_keep_one_recording() {
        let mut s = Sim::new();
        // Presses faster than anything finishes: holds just over the arming
        // time, taps under it, both keys.
        let mut t = 0;
        for i in 0..200u64 {
            let key = if i % 3 == 0 { OPTION } else { COMMAND };
            match i % 4 {
                0 => s.tap(t, key),
                1 => {
                    s.down(t, key);
                    s.up(t + 210, key);
                }
                2 => {
                    s.tap(t, key);
                    s.down(t + 150, key);
                    s.up(t + 190, key);
                }
                _ => {
                    s.down(t, key);
                    s.down(t + 100, ESC);
                    s.up(t + 120, ESC);
                    s.up(t + 300, key);
                }
            }
            assert!(!(s.local && s.wispr != WisprSim::Idle));
            if i % 7 == 0 && s.finishing > 0 {
                s.transcribed(i % 2 == 0);
            }
            t += 50 + (i * 37) % 300;
        }
        s.settle(t + 1_000);
        if s.recording() {
            // Only a hands-free dictation is left, and a press ends it.
            s.down(t + 10_000, COMMAND);
            s.up(t + 10_050, COMMAND);
            s.advance(t + 12_000);
        }
        assert!(!s.recording());
        assert!(!s.ptt);
    }

    /// Random presses of both keys and Escape, with the model finishing at
    /// random times: the stand-ins assert the rules on every step.
    #[test]
    fn random_spam_of_both_keys() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut s = Sim::new();
        let keys = [OPTION, COMMAND, ESC, C];
        let mut held = std::collections::BTreeSet::new();
        let mut t = 0;
        for _ in 0..20_000 {
            t += rand(300);
            let key = keys[rand(4) as usize];
            let down = held.insert(key);
            if !down {
                held.remove(&key);
            }
            s.key(t, key, down);
            if s.finishing > 0 && rand(5) == 0 {
                s.transcribed(rand(2) == 0);
            }
            if rand(500) == 0 && s.local {
                s.feed(Input::LocalFailed("model died".into()));
            }
        }
        s.settle(t + 1_000);
        if s.recording() {
            s.down(t + 10_000, COMMAND);
            s.up(t + 10_050, COMMAND);
            s.advance(t + 12_000);
        }
        assert!(!s.recording());
        assert!(!s.ctl.local_bar, "the bar was left on a local dictation");
    }
}
