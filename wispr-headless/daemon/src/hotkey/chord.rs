//! What the keys going up and down mean for a dictation, by the rules of
//! Wispr's own shortcuts: a binding matches when exactly its keys are held
//! (so RightAlt does not fire while Ctrl is down too). A hold binding is
//! push-to-talk, a toggle binding is hands-free: one press starts, the next
//! press of any binding ends it. Holding a hold binding and adding keys that
//! make a toggle binding turns the dictation hands-free, as Wispr's Fn+Space
//! does. Any other key joining soon after the press means the keys were a
//! shortcut of something else (RightAlt+E, Fn+F1), and the dictation is
//! cancelled; later, other keys are ignored, so typing while dictating does
//! not lose it. Platform-independent: hotkey/evdev.rs and hotkey/mac.rs feed
//! it.

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
}

/// What the shortcut did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// It went down: start recording.
    Pressed,
    /// The dictation goes on after the keys come up; the next press of a
    /// binding ends it with `Released`.
    HandsFree,
    /// Stop recording and transcribe: the hold ended, or a press ended a
    /// hands-free dictation.
    Released,
    /// It was not a dictation after all: stop and drop the recording.
    Cancelled,
}

impl Hotkey {
    /// As sent to the shim.
    pub fn name(self) -> &'static str {
        match self {
            Hotkey::Pressed => "pressed",
            Hotkey::HandsFree => "handsFree",
            Hotkey::Released => "released",
            Hotkey::Cancelled => "cancelled",
        }
    }
}

/// Another key joining within this time of the press cancels the dictation.
/// The same patience as the early start (EARLY_START_PATIENCE in main.rs).
pub const CANCEL_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    /// Push-to-talk on `binding`, pressed at `since`.
    Holding { binding: usize, since: Instant },
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
    cancel_window: Duration,
}

impl Chord {
    pub fn new(bindings: Vec<Binding>, cancel_window: Duration) -> Chord {
        Chord {
            bindings,
            held: BTreeSet::new(),
            state: State::Idle,
            cancel_window,
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
        if down {
            self.pressed(now)
        } else {
            self.released(key)
        }
    }

    fn pressed(&mut self, now: Instant) -> &'static [Hotkey] {
        match self.state {
            State::Idle => match self.exact() {
                Some(b) if self.bindings[b].mode == Mode::Hold => {
                    self.state = State::Holding {
                        binding: b,
                        since: now,
                    };
                    &[Hotkey::Pressed]
                }
                Some(_) => {
                    self.state = State::Latched;
                    &[Hotkey::Pressed, Hotkey::HandsFree]
                }
                None => &[],
            },
            State::Holding { binding, since } => {
                if let Some(b) = self.exact() {
                    if self.bindings[b].mode == Mode::Toggle {
                        self.state = State::Latched;
                        return &[Hotkey::HandsFree];
                    }
                }
                if self.partial() || now.duration_since(since) >= self.cancel_window {
                    &[]
                } else {
                    self.state = State::Settling { binding };
                    &[Hotkey::Cancelled]
                }
            }
            State::Latched => match self.exact() {
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
        match self.state {
            State::Holding { binding, .. } if self.bindings[binding].keys.contains(&key) => {
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
                chord: Chord::new(bindings, CANCEL_WINDOW),
                t0: Instant::now(),
            }
        }

        fn down(&mut self, ms: u64, key: Key) -> Vec<Hotkey> {
            let now = self.t0 + Duration::from_millis(ms);
            self.chord.key(key, true, now).to_vec()
        }

        fn up(&mut self, ms: u64, key: Key) -> Vec<Hotkey> {
            let now = self.t0 + Duration::from_millis(ms);
            self.chord.key(key, false, now).to_vec()
        }
    }

    use Hotkey::*;

    #[test]
    fn hold_is_push_to_talk() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Pressed]);
        // Auto-repeat and a second down change nothing.
        assert_eq!(r.down(30, ALT), []);
        assert_eq!(r.up(2000, ALT), [Released]);
        assert_eq!(r.up(2001, ALT), []);
        assert_eq!(r.down(3000, ALT), [Pressed]);
    }

    #[test]
    fn another_key_soon_after_the_press_cancels() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Pressed]);
        assert_eq!(r.down(120, E), [Cancelled]);
        // Letting go of E while still on RightAlt does not start again.
        assert_eq!(r.up(150, E), []);
        assert_eq!(r.up(200, ALT), []);
        assert_eq!(r.down(900, ALT), [Pressed]);
    }

    #[test]
    fn other_keys_later_on_are_ignored() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, ALT), [Pressed]);
        assert_eq!(r.down(800, E), []);
        assert_eq!(r.up(850, E), []);
        assert_eq!(r.up(3000, ALT), [Released]);
    }

    #[test]
    fn a_binding_matches_only_its_exact_keys() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.down(0, CTRL), []);
        assert_eq!(r.down(10, ALT), []);
        assert_eq!(r.up(20, ALT), []);
        assert_eq!(r.up(30, CTRL), []);
        assert_eq!(r.down(40, ALT), [Pressed]);
    }

    #[test]
    fn a_chord_starts_on_its_last_key_and_ends_on_its_first_release() {
        let mut r = Run::new(vec![hold(&[CTRL, ALT])]);
        assert_eq!(r.down(0, CTRL), []);
        assert_eq!(r.down(40, ALT), [Pressed]);
        assert_eq!(r.up(1000, CTRL), [Released]);
        assert_eq!(r.up(1010, ALT), []);
    }

    #[test]
    fn toggle_is_hands_free() {
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
        assert_eq!(r.down(0, ALT), [Pressed]);
        assert_eq!(r.down(200, SPACE), [HandsFree]);
        assert_eq!(r.up(300, SPACE), []);
        assert_eq!(r.up(320, ALT), []);
        // The push-to-talk key ends it; Space joining afterwards does nothing.
        assert_eq!(r.down(4000, ALT), [Released]);
        assert_eq!(r.down(4100, SPACE), []);
        assert_eq!(r.up(4200, ALT), []);
        assert_eq!(r.up(4210, SPACE), []);
        assert_eq!(r.down(5000, ALT), [Pressed]);
        assert_eq!(r.up(6000, ALT), [Released]);
        // The toggle chord from rest starts hands-free right away.
        assert_eq!(r.down(7000, SPACE), []);
        assert_eq!(r.down(7010, ALT), [Pressed, HandsFree]);
    }

    #[test]
    fn keys_on_the_way_to_a_longer_binding_do_not_cancel() {
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE, E])]);
        assert_eq!(r.down(0, ALT), [Pressed]);
        assert_eq!(r.down(50, SPACE), []);
        assert_eq!(r.down(90, E), [HandsFree]);
        let mut r = Run::new(vec![hold(&[ALT]), toggle(&[ALT, SPACE, E])]);
        assert_eq!(r.down(0, ALT), [Pressed]);
        assert_eq!(r.down(50, SPACE), []);
        assert_eq!(r.down(90, CTRL), [Cancelled]);
    }

    #[test]
    fn a_release_never_seen_going_down_is_ignored() {
        let mut r = Run::new(vec![hold(&[ALT])]);
        assert_eq!(r.up(0, ALT), []);
        assert_eq!(r.up(0, E), []);
        assert_eq!(r.down(10, ALT), [Pressed]);
    }
}
