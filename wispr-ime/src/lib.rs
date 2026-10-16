#![forbid(unsafe_code)]
//! wispr-ime: an IBus front for GNOME that hands input to fcitx5.
//!
//! GNOME's mutter finds its input method through an IBus address file and the
//! `org.freedesktop.IBus` bus name. fcitx5 normally provides both, but its
//! NixOS `ignoreUserConfig` setting stops it from writing the file, and an
//! in-process fcitx5 add-on cannot be written in safe Rust. This daemon takes
//! over both roles from outside: it speaks IBus to mutter, forwards everything
//! to fcitx5 over fcitx5's own D-Bus frontend, and adds one call that commits
//! text into the focused field for dictation.

pub mod addrfile;
pub mod daemon;
pub mod fcitx;
pub mod ibus;
pub mod inject;
pub mod state;
pub mod wire;
