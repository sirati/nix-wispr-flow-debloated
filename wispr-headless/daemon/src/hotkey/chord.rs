//! What the keys going up and down mean for a dictation, by the rules of
//! Wispr's own shortcuts: a binding matches when exactly its keys are held
//! (so RightAlt does not fire while Ctrl is down too).
//!
//! A hold binding is push-to-talk: held alone for `ARM` it is a dictation
//! until it comes up. A short tap on it does nothing, unless the same
//! binding goes down again within the double-press time (`double_press`,
//! measured from the tap's release): then the dictation is hands-free and
//! goes on without the key. A toggle binding is hands-free at once. While a
//! dictation is hands-free, a press of any binding ends it, and Escape
//! cancels it. Holding a hold binding and adding keys that make a toggle
//! binding turns the dictation hands-free, as Wispr's Fn+Space does.
//!
//! A hold binding is often a modifier that also makes shortcuts (Right
//! Command+C, Right Option+3), so until it is a dictation (`ARM`, or the
//! second press) the caller may record but shows nothing. Any other key while
//! it is held (the keys of a longer binding aside), or before the second
//! press, means a shortcut of something else, and the dictation is
//! cancelled. Other keys typed during a hands-free dictation are ignored.
//! Platform-independent: hotkey/evdev.rs and hotkey/mac.rs feed it.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// A key's code on this platform (see keys.rs).
pub type Key = u16;

/// Not a key: tells the chord that the caller ended the dictation by itself
/// (hotkey::Reset), so it stops waiting for the keys that would end it.
pub const RESET: Key = Key::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Push-to-talk: records while the keys are held; double press for
    /// hands-free.
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
    /// binding ends it with `Released`. Right after `Pressed`: it starts
    /// hands-free.
    HandsFree,
    /// Stop recording and transcribe: the hold ended, or a press ended a
    /// hands-free dictation.
    Released,
    /// It was not a dictation after all (a tap, or a shortcut of something
    /// else), or Escape: stop and drop the recording.
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

/// Default time between a tap's release and the second press that makes a
/// hands-free dictation.
pub const DOUBLE_PRESS: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    /// A hold binding is down since `since`; a dictation once it is `ARM`
    /// old.
    Arming { since: Instant },
    /// A hold binding was tapped; a second press before `until` makes a
    /// hands-free dictation, else it was nothing.
    Tapped { until: Instant },
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
    double: Duration,
    escape: Option<Key>,
}

impl Chord {
    pub fn new(bindings: Vec<Binding>, arm: Duration) -> Chord {
        Chord {
            bindings,
            held: BTreeSet::new(),
            state: State::Idle,
            owner: 0,
            arm,
            double: Duration::ZERO,
            escape: None,
        }
    }

    /// Two presses this close make a hands-free dictation (zero: off, a tap
    /// is always nothing).
    pub fn double_press(mut self, double: Duration) -> Chord {
        self.double = double;
        self
    }

    /// The key that cancels a hands-free dictation.
    pub fn escape(mut self, key: Option<Key>) -> Chord {
        self.escape = key;
        self
    }

    /// The binding the events are about: the one the dictation started on.
    pub fn owner(&self) -> Option<&Binding> {
        self.bindings.get(self.owner)
    }

    /// When `tick` has something to say.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Arming { since } => Some(since + self.arm),
            State::Tapped { until } => Some(until),
            _ => None,
        }
    }

    /// Time passed: a hold binding held long enough becomes a dictation, and
    /// a tap without a second press was nothing.
    pub fn tick(&mut self, now: Instant) -> Vec<Hotkey> {
        match self.state {
            State::Arming { since } if now >= since + self.arm => {
                self.state = State::Holding;
                vec![Hotkey::Pressed]
            }
            State::Tapped { until } if now >= until => {
                self.state = State::Idle;
                vec![Hotkey::Cancelled]
            }
            _ => Vec::new(),
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
    pub fn key(&mut self, key: Key, down: bool, now: Instant) -> Vec<Hotkey> {
        if key == RESET {
            self.reset();
            return Vec::new();
        }
        let changed = if down {
            self.held.insert(key)
        } else {
            self.held.remove(&key)
        };
        if !changed {
            return Vec::new();
        }
        // Time that passed counts before this key: a hold old enough is a
        // dictation, a tap too long ago is over.
        let mut out = self.tick(now);
        out.extend(if down {
            self.pressed(key, now)
        } else {
            self.released(key, now)
        });
        out
    }

    /// The caller ended the dictation (a time limit, an error, its bar): the
    /// keys that are still down end nothing, and the next press starts anew.
    fn reset(&mut self) {
        self.state = match self.state {
            State::Holding | State::Latched if !self.all_up(self.owner) => State::Settling {
                binding: self.owner,
            },
            State::Holding | State::Latched => State::Idle,
            other => other,
        };
    }

    fn pressed(&mut self, key: Key, now: Instant) -> Vec<Hotkey> {
        let exact = self.exact();
        let toggle = exact.filter(|&b| self.bindings[b].mode == Mode::Toggle);
        match self.state {
            State::Idle => match exact {
                Some(b) if self.bindings[b].mode == Mode::Hold => {
                    self.owner = b;
                    self.state = State::Arming { since: now };
                    vec![Hotkey::Down]
                }
                Some(b) => {
                    self.owner = b;
                    self.state = State::Latched;
                    vec![Hotkey::Pressed, Hotkey::HandsFree]
                }
                None => Vec::new(),
            },
            State::Arming { .. } | State::Holding => {
                let arming = matches!(self.state, State::Arming { .. });
                if let Some(b) = toggle {
                    self.state = State::Latched;
                    if arming {
                        // It starts as the hands-free binding's dictation.
                        self.owner = b;
                        return vec![Hotkey::Pressed, Hotkey::HandsFree];
                    }
                    return vec![Hotkey::HandsFree];
                }
                if self.partial() && Some(key) != self.escape {
                    return Vec::new();
                }
                self.state = State::Settling {
                    binding: self.owner,
                };
                vec![Hotkey::Cancelled]
            }
            State::Tapped { .. } => {
                if exact == Some(self.owner) {
                    self.state = State::Latched;
                    return vec![Hotkey::Pressed, Hotkey::HandsFree];
                }
                if exact.is_none() && self.partial() && Some(key) != self.escape {
                    // On the way to a binding: wait for the rest.
                    return Vec::new();
                }
                // Something else: the tap was nothing, and this press is
                // looked at afresh.
                self.state = State::Idle;
                let mut out = vec![Hotkey::Cancelled];
                out.extend(self.pressed(key, now));
                out
            }
            State::Latched => match exact {
                Some(b) => {
                    self.state = State::Settling { binding: b };
                    vec![Hotkey::Released]
                }
                None if Some(key) == self.escape => {
                    self.state = State::Idle;
                    vec![Hotkey::Cancelled]
                }
                None => Vec::new(),
            },
            State::Settling { .. } => Vec::new(),
        }
    }

    fn released(&mut self, key: Key, now: Instant) -> Vec<Hotkey> {
        let owned = self.bindings[self.owner].keys.contains(&key);
        match self.state {
            // A tap: perhaps the first of two.
            State::Arming { .. } if owned => {
                if self.double.is_zero() {
                    self.state = State::Idle;
                    vec![Hotkey::Cancelled]
                } else {
                    self.state = State::Tapped {
                        until: now + self.double,
                    };
                    Vec::new()
                }
            }
            State::Holding if owned => {
                self.state = State::Idle;
                vec![Hotkey::Released]
            }
            State::Settling { binding } if self.all_up(binding) => {
                self.state = State::Idle;
                Vec::new()
            }
            _ => Vec::new(),
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
    const ESC: Key = 1;
    const META: Key = 126;

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

        /// With double press for hands-free and Escape, as the daemon runs it.
        fn full(bindings: Vec<Binding>) -> Run {
            Run {
                chord: Chord::new(bindings, ARM)
                    .double_press(DOUBLE_PRESS)
                    .escape(Some(ESC)),
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

    #[test]
    fn a_double_press_is_hands_free_and_a_tap_alone_is_nothing() {
        let mut r = Run::full(vec![hold(&[ALT])]);
        // A tap: recording may start, nothing is shown, and nothing is left.
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(100, ALT), []);
        assert_eq!(r.chord.deadline(), Some(r.at(450)));
        assert_eq!(r.wait(449), []);
        assert_eq!(r.wait(450), [Cancelled]);
        assert_eq!(r.chord.deadline(), None);
        // Two presses: hands-free from the second, which ends nothing when it
        // comes up.
        assert_eq!(r.down(1000, ALT), [Down]);
        assert_eq!(r.up(1080, ALT), []);
        assert_eq!(r.down(1300, ALT), [Pressed, HandsFree]);
        assert_eq!(r.up(1400, ALT), []);
        assert_eq!(r.wait(5000), []);
        // The next press ends it; its release starts nothing.
        assert_eq!(r.down(6000, ALT), [Released]);
        assert_eq!(r.up(6100, ALT), []);
        assert_eq!(r.wait(7000), []);
        assert_eq!(r.down(8000, ALT), [Down]);
    }

    #[test]
    fn a_second_press_too_late_is_a_new_press() {
        let mut r = Run::full(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(100, ALT), []);
        // No timer came; the late press still ends the tap first.
        assert_eq!(r.down(600, ALT), [Cancelled, Down]);
        assert_eq!(r.wait(800), [Pressed]);
        assert_eq!(r.up(900, ALT), [Released]);
    }

    #[test]
    fn a_hold_is_push_to_talk_with_double_press_on() {
        let mut r = Run::full(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.wait(200), [Pressed]);
        assert_eq!(r.up(2000, ALT), [Released]);
        // A hold's release opens no double-press window.
        assert_eq!(r.chord.deadline(), None);
        assert_eq!(r.down(2100, ALT), [Down]);
    }

    #[test]
    fn another_key_after_a_tap_ends_it() {
        let mut r = Run::full(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(100, ALT), []);
        assert_eq!(r.down(200, E), [Cancelled]);
        assert_eq!(r.up(250, E), []);
        assert_eq!(r.wait(1000), []);
    }

    #[test]
    fn escape_cancels_a_hands_free_dictation() {
        let mut r = Run::full(vec![hold(&[ALT]), toggle(&[F19])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(50, ALT), []);
        assert_eq!(r.down(150, ALT), [Pressed, HandsFree]);
        assert_eq!(r.up(200, ALT), []);
        // Other keys are typing.
        assert_eq!(r.down(300, E), []);
        assert_eq!(r.up(320, E), []);
        assert_eq!(r.down(1000, ESC), [Cancelled]);
        assert_eq!(r.up(1050, ESC), []);
        assert_eq!(r.down(2000, F19), [Pressed, HandsFree]);
        assert_eq!(r.up(2050, F19), []);
        assert_eq!(r.down(3000, ESC), [Cancelled]);
        assert_eq!(r.up(3050, ESC), []);
        // Escape while holding is another key: cancelled too.
        assert_eq!(r.down(4000, ALT), [Down]);
        assert_eq!(r.wait(4200), [Pressed]);
        assert_eq!(r.down(4500, ESC), [Cancelled]);
    }

    #[test]
    fn any_binding_ends_a_hands_free_dictation() {
        let local = Binding {
            backend: Some("local".into()),
            ..hold(&[META])
        };
        let wispr = Binding {
            backend: Some("wispr".into()),
            ..hold(&[ALT])
        };
        let mut r = Run::full(vec![wispr, local]);
        // Double press on Meta: local, hands-free.
        assert_eq!(r.down(0, META), [Down]);
        assert_eq!(r.up(60, META), []);
        assert_eq!(r.down(200, META), [Pressed, HandsFree]);
        assert_eq!(r.chord.owner().unwrap().backend.as_deref(), Some("local"));
        assert_eq!(r.up(260, META), []);
        // Holding Alt ends it and does not start Wispr's.
        assert_eq!(r.down(3000, ALT), [Released]);
        assert_eq!(r.wait(3500), []);
        assert_eq!(r.up(4000, ALT), []);
        assert_eq!(r.down(5000, ALT), [Down]);
        assert_eq!(r.chord.owner().unwrap().backend.as_deref(), Some("wispr"));
    }

    #[test]
    fn a_tap_then_another_binding_starts_that_one() {
        let mut r = Run::full(vec![hold(&[ALT]), hold(&[META])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(60, ALT), []);
        assert_eq!(r.down(100, META), [Cancelled, Down]);
        assert_eq!(r.chord.owner().unwrap().keys, [META]);
        assert_eq!(r.wait(300), [Pressed]);
        assert_eq!(r.up(500, META), [Released]);
    }

    #[test]
    fn a_reset_forgets_the_dictation_the_caller_ended() {
        let mut r = Run::full(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Down]);
        assert_eq!(r.up(50, ALT), []);
        assert_eq!(r.down(100, ALT), [Pressed, HandsFree]);
        assert_eq!(r.up(150, ALT), []);
        // Ended by the daemon (time limit): the next press starts afresh.
        assert_eq!(r.down(200, RESET), []);
        assert_eq!(r.down(1000, ALT), [Down]);
        assert_eq!(r.wait(1200), [Pressed]);
        // Ended while still held: its release says nothing.
        assert_eq!(r.down(1500, RESET), []);
        assert_eq!(r.up(1600, ALT), []);
        assert_eq!(r.down(2000, ALT), [Down]);
    }

    /// Whatever the keys do, every dictation that starts ends, and a new one
    /// never starts while one runs.
    #[test]
    fn random_key_spam_keeps_dictations_paired() {
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let keys = [ALT, META, ESC, E, SPACE];
        let mut r = Run::full(vec![hold(&[ALT]), hold(&[META]), toggle(&[ALT, SPACE])]);
        let mut t = 0;
        let mut dictating = false;
        // A Down that is not a dictation yet.
        let mut pending = false;
        let mut held = BTreeSet::new();
        let mut check = |events: Vec<Hotkey>, t: u64| {
            for e in events {
                match e {
                    Down => {
                        assert!(!dictating && !pending, "Down while busy at {t}");
                        pending = true;
                    }
                    Pressed => {
                        assert!(!dictating, "Pressed while dictating at {t}");
                        dictating = true;
                        pending = false;
                    }
                    HandsFree => assert!(dictating, "HandsFree without a dictation at {t}"),
                    Released => {
                        assert!(dictating, "Released without a dictation at {t}");
                        dictating = false;
                    }
                    Cancelled => {
                        assert!(dictating || pending, "Cancelled for nothing at {t}");
                        dictating = false;
                        pending = false;
                    }
                }
            }
            (dictating, pending)
        };
        for _ in 0..20_000 {
            t += rand(400);
            check(r.wait(t), t);
            let key = keys[rand(keys.len() as u64) as usize];
            let down = held.insert(key);
            if !down {
                held.remove(&key);
            }
            check(r.chord.key(key, down, r.at(t)), t);
        }
        // Let go of everything and wait: nothing is left but a hands-free
        // dictation, which the next press ends.
        for key in held.clone() {
            check(r.chord.key(key, false, r.at(t)), t);
        }
        let (dictating, pending) = check(r.wait(t + 10_000), t + 10_000);
        assert!(!pending, "an armed press was left");
        if dictating {
            assert_eq!(r.down(t + 20_000, META), [Released]);
        }
    }
}
