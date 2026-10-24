//! What the keys going up and down mean for a dictation, by the rules of
//! Wispr's own shortcuts: a binding matches when exactly its keys are held
//! (so RightAlt does not fire while Ctrl is down too). A hold binding is
//! push-to-talk, a toggle binding is hands-free: one press starts, the next
//! press of any binding ends it. Holding a hold binding and adding keys that
//! make a toggle binding turns the dictation hands-free, as Wispr's Fn+Space
//! does.
//!
//! A hold binding is often a modifier that also makes shortcuts (Right
//! Command+C, Right Option+3), so it only becomes a dictation once held
//! alone for `ARM`; until then the caller may record, but shows nothing.
//! Any other key while it is held (the keys of a longer binding aside) means
//! a shortcut of something else, and the dictation is cancelled. Keys typed
//! during a hands-free dictation are ignored. Platform-independent:
//! hotkey/evdev.rs and hotkey/mac.rs feed it.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// A key's code on this platform (see keys.rs).
pub type Key = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Push-to-talk: records while the keys are held.
    Hold,
    /// Hands-free: a press starts, the next press of any binding stops.
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Sorted, each key once.
    pub keys: Vec<Key>,
    pub mode: Mode,
    /// Where its dictations go ("wispr", "local"); None for the default.
    pub backend: Option<String>,
}

/// What the shortcut did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// A hold binding went down. Not a dictation yet: record, so nothing
    /// said is lost, but show nothing.
    Down,
    /// It is a dictation: start, or keep what `Down` recorded.
    Pressed,
    /// The dictation goes on after the keys come up; the next press of a
    /// binding ends it with `Released`.
    HandsFree,
    /// Stop recording and transcribe: the hold ended, or a press ended a
    /// hands-free dictation.
    Released,
    /// It was not a dictation after all (a tap, or a shortcut of something
    /// else): stop and drop the recording.
    Cancelled,
}

impl Hotkey {
    pub fn name(self) -> &'static str {
        match self {
            Hotkey::Down => "down",
            Hotkey::Pressed => "pressed",
            Hotkey::HandsFree => "handsFree",
            Hotkey::Released => "released",
            Hotkey::Cancelled => "cancelled",
        }
    }
}

/// How long a hold binding has to be held alone to be a dictation. Shortcuts
/// with a modifier take less between the two keys.
pub const ARM: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    /// A hold binding is down since `since`; a dictation once it is `ARM`
    /// old.
    Arming { since: Instant },
    /// Push-to-talk.
    Holding,
    /// Hands-free.
    Latched,
    /// Ended or cancelled: nothing starts again until `binding`'s keys are
    /// all up, so the press that ended it does not also start a new one.
    Settling { binding: usize },
}

pub struct Chord {
    bindings: Vec<Binding>,
    held: BTreeSet<Key>,
    state: State,
    /// The binding the current (or last) dictation started on.
    owner: usize,
    arm: Duration,
}

impl Chord {
    pub fn new(bindings: Vec<Binding>, arm: Duration) -> Chord {
        Chord {
            bindings,
            held: BTreeSet::new(),
            state: State::Idle,
            owner: 0,
            arm,
        }
    }

    /// The binding the events are about: the one the dictation started on.
    pub fn owner(&self) -> Option<&Binding> {
        self.bindings.get(self.owner)
    }

    /// When `tick` has something to say.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Arming { since } => Some(since + self.arm),
            _ => None,
        }
    }

    /// Time passed: a hold binding held long enough becomes a dictation.
    pub fn tick(&mut self, now: Instant) -> &'static [Hotkey] {
        match self.state {
            State::Arming { since } if now >= since + self.arm => {
                self.state = State::Holding;
                &[Hotkey::Pressed]
            }
            _ => &[],
        }
    }

    /// The binding whose keys are exactly the ones held.
    fn exact(&self) -> Option<usize> {
        self.bindings
            .iter()
            .position(|b| b.keys.iter().eq(self.held.iter()))
    }

    /// Whether the held keys are all part of one binding.
    fn partial(&self) -> bool {
        self.bindings
            .iter()
            .any(|b| self.held.iter().all(|k| b.keys.binary_search(k).is_ok()))
    }

    fn all_up(&self, binding: usize) -> bool {
        !self.bindings[binding]
            .keys
            .iter()
            .any(|k| self.held.contains(k))
    }

    /// `key` went down or up at `now`. Repeats and keys released that were
    /// never seen going down change nothing.
    pub fn key(&mut self, key: Key, down: bool, now: Instant) -> &'static [Hotkey] {
        let changed = if down {
            self.held.insert(key)
        } else {
            self.held.remove(&key)
        };
        if !changed {
            return &[];
        }
        // A hold that is old enough counts as one before this key.
        if !self.tick(now).is_empty() {
            return if down {
                match self.pressed(now) {
                    [] => &[Hotkey::Pressed],
                    [Hotkey::HandsFree] => &[Hotkey::Pressed, Hotkey::HandsFree],
                    _ => &[Hotkey::Pressed, Hotkey::Cancelled],
                }
            } else {
                match self.released(key) {
                    [] => &[Hotkey::Pressed],
                    _ => &[Hotkey::Pressed, Hotkey::Released],
                }
            };
        }
        if down {
            self.pressed(now)
        } else {
            self.released(key)
        }
    }

    fn pressed(&mut self, now: Instant) -> &'static [Hotkey] {
        let exact = self.exact();
        let toggle = exact.filter(|&b| self.bindings[b].mode == Mode::Toggle);
        match self.state {
            State::Idle => match exact {
                Some(b) if self.bindings[b].mode == Mode::Hold => {
                    self.owner = b;
                    self.state = State::Arming { since: now };
                    &[Hotkey::Down]
                }
                Some(b) => {
                    self.owner = b;
                    self.state = State::Latched;
                    &[Hotkey::Pressed, Hotkey::HandsFree]
                }
                None => &[],
            },
            State::Arming { .. } | State::Holding => {
                let arming = matches!(self.state, State::Arming { .. });
                if let Some(b) = toggle {
                    self.state = State::Latched;
                    if arming {
                        // It starts as the hands-free binding's dictation.
                        self.owner = b;
                        return &[Hotkey::Pressed, Hotkey::HandsFree];
                    }
                    return &[Hotkey::HandsFree];
                }
                if self.partial() {
                    return &[];
                }
                self.state = State::Settling {
                    binding: self.owner,
                };
                &[Hotkey::Cancelled]
            }
            State::Latched => match exact {
                Some(b) => {
                    self.state = State::Settling { binding: b };
                    &[Hotkey::Released]
                }
                None => &[],
            },
            State::Settling { .. } => &[],
        }
    }

    fn released(&mut self, key: Key) -> &'static [Hotkey] {
        let owned = self.bindings[self.owner].keys.contains(&key);
        match self.state {
            // A tap.
            State::Arming { .. } if owned => {
                self.state = State::Idle;
                &[Hotkey::Cancelled]
            }
            State::Holding if owned => {
                self.state = State::Idle;
                &[Hotkey::Released]
            }
            State::Settling { binding } if self.all_up(binding) => {
                self.state = State::Idle;
                &[]
            }
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALT: Key = 100;
    const CTRL: Key = 29;
    const SPACE: Key = 57;
    const E: Key = 18;
    const F19: Key = 189;

    fn hold(keys: &[Key]) -> Binding {
        let mut keys = keys.to_vec();
        keys.sort_unstable();
        Binding {
            keys,
            mode: Mode::Hold,
            backend: None,
        }
    }

    fn toggle(keys: &[Key]) -> Binding {
        Binding {
            mode: Mode::Toggle,
            ..hold(keys)
        }
    }

    /// Plays key events at the given milliseconds and collects the result.
    struct Run {
        chord: Chord,
        t0: Instant,
    }

    impl Run {
        fn new(bindings: Vec<Binding>) -> Run {
            Run {
                chord: Chord::new(bindings, ARM),
                t0: Instant::now(),
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        fn down(&mut self, ms: u64, key: Key) -> Vec<Hotkey> {
            self.chord.key(key, true, self.at(ms)).to_vec()
        }

        fn up(&mut self, ms: u64, key: Key) -> Vec<Hotkey> {
            self.chord.key(key, false, self.at(ms)).to_vec()
        }

        /// The caller's timer, when the chord asks for it.
        fn wait(&mut self, ms: u64) -> Vec<Hotkey> {
            let now = self.at(ms);
            match self.chord.deadline() {
                Some(d) if d <= now => self.chord.tick(now).to_vec(),
                _ => Vec::new(),
            }
        }
    }

    use Hotkey::*;

    #[test]
    fn hold_is_push_to_talk_once_armed() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.chord.deadline(), Some(r.at(200)));
        assert_eq!(r.wait(150), []);
        assert_eq!(r.wait(200), [Pressed]);
        assert_eq!(r.chord.deadline(), None);
        // Auto-repeat and a second down change nothing.
        assert_eq!(r.down(300, ALT), []);
        assert_eq!(r.up(2000, ALT), [Released]);
        assert_eq!(r.up(2001, ALT), []);
        assert_eq!(r.down(3000, ALT), [Down]);
    }

    #[test]
    fn a_tap_is_no_dictation() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(120, ALT), [Cancelled]);
        assert_eq!(r.wait(500), []);
    }

    #[test]
    fn a_shortcut_with_the_modifier_is_no_dictation() {
        // Right Command+C: C before the hold is armed.
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.down(90, E), [Cancelled]);
        // Letting go of E while still on the modifier does not start again.
        assert_eq!(r.up(150, E), []);
        assert_eq!(r.wait(400), []);
        assert_eq!(r.up(500, ALT), []);
        assert_eq!(r.down(900, ALT), [Down]);
    }

    #[test]
    fn another_key_during_the_dictation_cancels_it() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.wait(200), [Pressed]);
        assert_eq!(r.down(3000, E), [Cancelled]);
        assert_eq!(r.up(3050, E), []);
        assert_eq!(r.up(3100, ALT), []);
    }

    #[test]
    fn a_late_timer_still_arms_before_the_next_key() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        // No tick came in time; the release at 900 ms still ends a dictation.
        assert_eq!(r.up(900, ALT), [Pressed, Released]);
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.down(900, E), [Pressed, Cancelled]);
    }

    #[test]
    fn a_binding_matches_only_its_exact_keys() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, CTRL), []);
        assert_eq!(r.down(10, ALT), []);
        assert_eq!(r.up(20, ALT), []);
        assert_eq!(r.up(30, CTRL), []);
        assert_eq!(r.down(40, ALT), [Down]);
    }

    #[test]
    fn a_chord_starts_on_its_last_key_and_ends_on_its_first_release() {
        let mut r = Run::new(vec![hold(&[CTRL, ALT])]);
        assert_eq!(r.down(0, CTRL), []);
        assert_eq!(r.down(40, ALT), [Down]);
        assert_eq!(r.wait(240), [Pressed]);
        assert_eq!(r.up(1000, CTRL), [Released]);
        assert_eq!(r.up(1010, ALT), []);
    }

    #[test]
    fn toggle_is_hands_free_at_once() {
        let mut r = Run::new(vec![toggle(&[F19])]);
        assert_eq!(r.down(0, F19), [Pressed, HandsFree]);
        assert_eq!(r.up(50, F19), []);
        // Typing while dictating does not stop it.
        assert_eq!(r.down(100, E), []);
        assert_eq!(r.up(150, E), []);
        assert_eq!(r.down(5000, F19), [Released]);
        assert_eq!(r.up(5050, F19), []);
        assert_eq!(r.down(6000, F19), [Pressed, HandsFree]);
    }

    #[test]
    fn hold_becomes_hands_free_like_wispr_fn_space() {
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.wait(200), [Pressed]);
        assert_eq!(r.down(400, SPACE), [HandsFree]);
        assert_eq!(r.up(500, SPACE), []);
        assert_eq!(r.up(520, ALT), []);
        // The push-to-talk key ends it; Space joining afterwards does nothing.
        assert_eq!(r.down(4000, ALT), [Released]);
        assert_eq!(r.down(4100, SPACE), []);
        assert_eq!(r.up(4200, ALT), []);
        assert_eq!(r.up(4210, SPACE), []);
        assert_eq!(r.down(5000, ALT), [Down]);
        // Space before the hold is armed: hands-free straight away.
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.down(80, SPACE), [Pressed, HandsFree]);
        assert_eq!(r.chord.owner().unwrap().mode, Mode::Toggle);
        // The toggle chord from rest.
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE])]);
        assert_eq!(r.down(0, SPACE), []);
        assert_eq!(r.down(10, ALT), [Pressed, HandsFree]);
    }

    #[test]
    fn keys_on_the_way_to_a_longer_binding_do_not_cancel() {
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE, E])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.down(50, SPACE), []);
        assert_eq!(r.down(90, E), [Pressed, HandsFree]);
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE, E])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.down(50, SPACE), []);
        assert_eq!(r.down(90, CTRL), [Cancelled]);
    }

    #[test]
    fn events_name_the_binding_the_dictation_started_on() {
        let local = Binding {
            backend: Some("local".into()),
            ..hold(&[CTRL])
        };
        let wispr = Binding {
            backend: Some("wispr".into()),
            ..toggle(&[ALT])
        };
        let mut r = Run::new(vec![wispr, local]);
        assert_eq!(r.down(0, CTRL), [Down]);
        assert_eq!(r.chord.owner().unwrap().backend.as_deref(), Some("local"));
        assert_eq!(r.up(50, CTRL), [Cancelled]);
        assert_eq!(r.down(100, ALT), [Pressed, HandsFree]);
        assert_eq!(r.chord.owner().unwrap().backend.as_deref(), Some("wispr"));
        assert_eq!(r.up(150, ALT), []);
        // A hands-free dictation ends on any binding and stays its starter's.
        assert_eq!(r.down(900, CTRL), [Released]);
        assert_eq!(r.chord.owner().unwrap().backend.as_deref(), Some("wispr"));
    }

    #[test]
    fn a_release_never_seen_going_down_is_ignored() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.up(0, ALT), []);
        assert_eq!(r.up(0, E), []);
        assert_eq!(r.down(10, ALT), [Down]);
    }
}
