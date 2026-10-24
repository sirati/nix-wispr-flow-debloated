//! Which key types which character, from the X server's core keyboard
//! mapping (GetKeyboardMapping). On Wayland that is XWayland's copy of the
//! compositor's layout (KWin and Mutter keep it in step), and X keycodes
//! are evdev codes plus 8 on both, so the same table serves the uinput
//! keyboard on Wayland and XTEST on X11. Platform-independent; typing/x11.rs
//! reads the mapping.
//!
//! The core mapping lists, per keycode, group 1's levels 1 and 2 (plain,
//! Shift), group 2's levels 1 and 2, then group 1's levels 3 and 4 (AltGr,
//! AltGr+Shift). Only the active group's columns are used; a third or
//! fourth layout has no columns here, so nothing is typed with it (the
//! caller pastes instead). Dead keys and keypad keys never stand for a
//! character, so `'` is only typed with a key that gives `'` itself.

use std::collections::HashMap;

use super::keysyms;
use super::Piece;

/// X keycode = evdev code + 8.
pub const EVDEV_OFFSET: u8 = 8;

const SHIFT_L: u32 = 0xffe1;
const CONTROL_L: u32 = 0xffe3;
const LEVEL3_SHIFT: u32 = 0xfe03;
const RETURN: u32 = 0xff0d;
const TAB: u32 = 0xff09;
/// Right Alt, the usual AltGr; preferred over keycodes with no physical key
/// that some layouts also bind to level 3.
const RIGHT_ALT: u8 = 100 + EVDEV_OFFSET;

/// One key press, with the modifiers held for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    /// X keycode.
    pub keycode: u8,
    pub shift: bool,
    /// AltGr (ISO_Level3_Shift).
    pub level3: bool,
}

impl Stroke {
    fn cost(&self) -> (u8, u8) {
        (u8::from(self.level3) * 2 + u8::from(self.shift), self.keycode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Key(Stroke),
    /// Not in the layout.
    Missing(char),
}

pub struct Keymap {
    chars: HashMap<char, Stroke>,
    enter: Option<u8>,
    tab: Option<u8>,
    pub shift: u8,
    pub control: u8,
    pub level3: Option<u8>,
    /// Keycodes without any keysym, which XTEST can borrow for a character
    /// the layout lacks.
    pub spare: Vec<u8>,
    /// Keysyms per keycode, as the server reported them.
    pub per_keycode: u8,
}

impl Keymap {
    /// From GetKeyboardMapping for keycodes `min_keycode` and up, `per`
    /// keysyms each. `group` is the active XKB group (0 for the first
    /// layout); `caps` is Caps Lock, which swaps Shift for letters.
    pub fn from_core(min_keycode: u8, per: u8, keysyms: &[u32], group: u8, caps: bool) -> Keymap {
        let per_keycode = per.max(1);
        let rows: Vec<(u8, &[u32])> = keysyms
            .chunks(usize::from(per_keycode))
            .enumerate()
            .filter_map(|(i, row)| Some((u8::try_from(usize::from(min_keycode) + i).ok()?, row)))
            .collect();
        let at = |row: &[u32], col: usize| row.get(col).copied().unwrap_or(0);
        let find = |keysym: u32| -> Vec<u8> {
            rows.iter()
                .filter(|(_, row)| at(row, 0) == keysym)
                .map(|&(k, _)| k)
                .collect()
        };
        let level3 = {
            let keys = find(LEVEL3_SHIFT);
            keys.iter()
                .copied()
                .find(|&k| k == RIGHT_ALT)
                .or(keys.first().copied())
        };
        // (plain column, Shift column, AltGr) for the active group.
        let pairs: &[(usize, usize, bool)] = match group {
            0 => &[(0, 1, false), (4, 5, true)],
            1 => &[(2, 3, false)],
            _ => &[],
        };
        let mut chars: HashMap<char, Stroke> = HashMap::new();
        for &(keycode, row) in &rows {
            for &(plain, shifted, l3) in pairs {
                if l3 && level3.is_none() {
                    continue;
                }
                let (lower, upper) = (keysyms::char_of(at(row, plain)), keysyms::char_of(at(row, shifted)));
                // Caps Lock swaps the levels of a letter key.
                let letter = matches!((lower, upper), (Some(l), Some(u))
                    if l.is_lowercase() && l.to_uppercase().eq(std::iter::once(u)));
                let swap = caps && letter;
                for (c, shift) in [(lower, swap), (upper, !swap)] {
                    let Some(c) = c else { continue };
                    let stroke = Stroke {
                        keycode,
                        shift,
                        level3: l3,
                    };
                    let better = chars.get(&c).is_none_or(|old| stroke.cost() < old.cost());
                    if better {
                        chars.insert(c, stroke);
                    }
                }
            }
        }
        let spare = rows
            .iter()
            .filter(|(k, row)| *k > EVDEV_OFFSET && row.iter().all(|&s| s == 0))
            .map(|&(k, _)| k)
            .collect();
        Keymap {
            chars,
            enter: find(RETURN).first().copied(),
            tab: find(TAB).first().copied(),
            shift: find(SHIFT_L).first().copied().unwrap_or(42 + EVDEV_OFFSET),
            control: find(CONTROL_L).first().copied().unwrap_or(29 + EVDEV_OFFSET),
            level3,
            spare,
            per_keycode,
        }
    }

    /// The key that types `c`.
    pub fn stroke(&self, c: char) -> Option<Stroke> {
        self.chars.get(&c).copied()
    }

    /// The V of Ctrl+V: the key that types `v` in this layout (on QWERTY
    /// and QWERTZ the evdev KEY_V, on Dvorak another one).
    pub fn v(&self) -> u8 {
        self.chars
            .get(&'v')
            .map_or(47 + EVDEV_OFFSET, |s| s.keycode)
    }

    /// The keys that type `text`. A new line is Shift+Enter, so a chat app
    /// adds a line instead of sending the message.
    pub fn plan(&self, text: &str) -> Vec<Step> {
        let key = |keycode: Option<u8>, shift: bool, c: char| match keycode {
            Some(keycode) => Step::Key(Stroke {
                keycode,
                shift,
                level3: false,
            }),
            None => Step::Missing(c),
        };
        super::pieces(text)
            .map(|p| match p {
                Piece::Char(c) => self.stroke(c).map_or(Step::Missing(c), Step::Key),
                Piece::Newline => key(self.enter, true, '\n'),
                Piece::Tab => key(self.tab, false, '\t'),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core mapping of 8 columns from rows of keysyms, padded with NoSymbol.
    fn mapping(rows: &[(u8, &[u32])]) -> (u8, Vec<u32>) {
        let min = 8u8;
        let max = rows.iter().map(|r| r.0).max().unwrap() + 3;
        let mut keysyms = vec![0u32; usize::from(max - min + 1) * 8];
        for &(keycode, syms) in rows {
            let at = usize::from(keycode - min) * 8;
            keysyms[at..at + syms.len()].copy_from_slice(syms);
        }
        (min, keysyms)
    }

    const fn k(evdev: u8) -> u8 {
        evdev + EVDEV_OFFSET
    }

    /// A slice of a British layout, with German as the second group of
    /// the `z` key and AltGr on Right Alt plus a keycode with no key.
    fn gb_de() -> (u8, Vec<u32>) {
        mapping(&[
            (k(2), &[0x31, 0x21, 0x31, 0x21, 0xb9, 0xa1]), // 1 ! 1 ! ¹ ¡
            (k(5), &[0x34, 0x24, 0x34, 0x24, 0x20ac, 0xbc]), // 4 $ 4 $ € ¼
            (k(21), &[0x79, 0x59, 0x7a, 0x5a]),            // y Y z Z
            (k(44), &[0x7a, 0x5a, 0x79, 0x59]),            // z Z y Y
            (k(18), &[0x65, 0x45, 0x65, 0x45, 0xe9, 0xc9]), // e E e E é É
            (k(40), &[0xfe51, 0xfe57, 0x27, 0x22]),        // dead_acute; ' " in group 2
            (k(43), &[0x23, 0x7e, 0x23, 0x27]),            // # ~ # '
            (k(47), &[0x76, 0x56, 0x76, 0x56]),            // v V
            (k(57), &[0x20]),                              // space
            (k(28), &[0xff0d]),                            // Return
            (k(15), &[0xff09, 0xfe20]),                    // Tab ISO_Left_Tab
            (k(42), &[0xffe1]),                            // Shift_L
            (k(29), &[0xffe3]),                            // Control_L
            (k(84), &[0xfe03]),                            // no key, level 3
            (k(100), &[0xfe03]),                           // Right Alt, level 3
            (k(79), &[0xffb1, 0x31]),                      // KP_1 1 (NumLock)
        ])
    }

    fn stroke(keycode: u8, shift: bool, level3: bool) -> Option<Stroke> {
        Some(Stroke {
            keycode,
            shift,
            level3,
        })
    }

    #[test]
    fn the_first_group_types_with_shift_and_altgr() {
        let (min, syms) = gb_de();
        let m = Keymap::from_core(min, 8, &syms, 0, false);
        assert_eq!(m.stroke('y'), stroke(k(21), false, false));
        assert_eq!(m.stroke('Z'), stroke(k(44), true, false));
        assert_eq!(m.stroke('!'), stroke(k(2), true, false));
        assert_eq!(m.stroke('€'), stroke(k(5), false, true));
        assert_eq!(m.stroke('É'), stroke(k(18), true, true));
        assert_eq!(m.level3, Some(k(100)), "Right Alt over the keyless code");
        assert_eq!(m.shift, k(42));
        assert_eq!(m.control, k(29));
        // A dead key never types its accent; the apostrophe is on # here.
        assert_eq!(m.stroke('\''), None);
        assert_eq!(m.stroke('~'), stroke(k(43), true, false));
        // The keypad's 1 is not used: the main row's is.
        assert_eq!(m.stroke('1'), stroke(k(2), false, false));
        assert_eq!(m.stroke('ß'), None);
        assert!(m.spare.contains(&k(3)));
        assert!(!m.spare.contains(&k(2)));
    }

    #[test]
    fn the_second_group_has_no_altgr() {
        let (min, syms) = gb_de();
        let m = Keymap::from_core(min, 8, &syms, 1, false);
        assert_eq!(m.stroke('z'), stroke(k(21), false, false));
        assert_eq!(m.stroke('y'), stroke(k(44), false, false));
        assert_eq!(m.stroke('\''), stroke(k(40), false, false));
        assert_eq!(m.stroke('€'), None);
        let third = Keymap::from_core(min, 8, &syms, 2, false);
        assert_eq!(third.stroke('a'), None);
    }

    #[test]
    fn caps_lock_swaps_shift_for_letters_only() {
        let (min, syms) = gb_de();
        let m = Keymap::from_core(min, 8, &syms, 0, true);
        assert_eq!(m.stroke('y'), stroke(k(21), true, false));
        assert_eq!(m.stroke('Y'), stroke(k(21), false, false));
        assert_eq!(m.stroke('é'), stroke(k(18), true, true));
        assert_eq!(m.stroke('!'), stroke(k(2), true, false));
    }

    #[test]
    fn a_plan_types_new_lines_as_shift_enter_and_marks_what_is_missing() {
        let (min, syms) = gb_de();
        let m = Keymap::from_core(min, 8, &syms, 0, false);
        let plan = m.plan("Ye\r\n€ ß\t");
        assert_eq!(
            plan,
            [
                Step::Key(stroke(k(21), true, false).unwrap()),
                Step::Key(stroke(k(18), false, false).unwrap()),
                Step::Key(stroke(k(28), true, false).unwrap()),
                Step::Key(stroke(k(5), false, true).unwrap()),
                Step::Key(stroke(k(57), false, false).unwrap()),
                Step::Missing('ß'),
                Step::Key(stroke(k(15), false, false).unwrap()),
            ]
        );
        assert_eq!(m.v(), k(47));
    }

    #[test]
    fn a_layout_without_level_three_types_none_of_it() {
        let (min, mut syms) = gb_de();
        for s in syms.iter_mut().filter(|s| **s == 0xfe03) {
            *s = 0;
        }
        let m = Keymap::from_core(min, 8, &syms, 0, false);
        assert_eq!(m.level3, None);
        assert_eq!(m.stroke('€'), None);
        assert_eq!(m.stroke('4'), stroke(k(5), false, false));
    }
}
