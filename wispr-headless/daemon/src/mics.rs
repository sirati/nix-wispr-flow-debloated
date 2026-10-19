//! Which microphone to record: the one Wispr's own recorder would open.
//!
//! Wispr keeps a ranking of microphones (`prefs.user.rankedAudioDevices`)
//! and records from the first one that is connected; an entry "default"
//! stands for the system default. Its device IDs are Chromium's, so they
//! mean nothing to ffmpeg or PipeWire; the shim passes each entry's name (and
//! Chromium's raw label, when Wispr's page has seen the device) and this
//! module matches those against the system's inputs by name. Nothing matched,
//! or "default" ranked first: the system default.

use std::process::{Command, Stdio};

use serde::Deserialize;

/// One entry of Wispr's ranking, as the shim sends it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Ranked {
    /// Wispr's display name ("Built-in mic (recommended)", "Headset").
    pub name: String,
    /// Chromium's label ("MacBook Pro Microphone (Built-in)"), if known.
    #[serde(default)]
    pub label: Option<String>,
    /// The "default" entry: the system default input.
    #[serde(default)]
    pub default: bool,
}

/// An input the recorder can open.
#[derive(Debug, Clone, PartialEq)]
pub struct Native {
    /// What the capture command takes: an AVFoundation device name, or a
    /// PipeWire node name.
    pub id: String,
    /// Its human name: the AVFoundation name or the node description, which
    /// is what Chromium shows as the label.
    pub name: String,
}

/// A name reduced the way Wispr does for display: lower case, without
/// "Default - ", without Chromium's trailing "(Built-in)", "(Bluetooth)",
/// "(046d:0825)" or "(2)" groups, without the word "microphone".
pub fn normalize(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    if let Some(rest) = s.strip_prefix("default - ") {
        s = rest.to_string();
    }
    loop {
        let t = s.trim_end();
        match (t.ends_with(')'), t.rfind('(')) {
            (true, Some(open)) => s = t[..open].to_string(),
            _ => break,
        }
    }
    s.split_whitespace()
        .filter(|w| *w != "microphone")
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Mac's own microphone (AVFoundation calls it "MacBook Pro Microphone").
fn built_in(n: &Native) -> bool {
    let name = n.name.to_lowercase();
    name.contains("built-in")
        || ["macbook", "imac", "mac mini", "mac studio", "mac pro"]
            .iter()
            .any(|m| name.starts_with(m))
}

/// The input Wispr's ranking picks among `native`, or None for the system
/// default.
pub fn choose<'a>(ranked: &[Ranked], native: &'a [Native]) -> Option<&'a Native> {
    for r in ranked {
        if r.default {
            return None;
        }
        let wanted: Vec<String> = r
            .label
            .iter()
            .chain(std::iter::once(&r.name))
            .map(|n| normalize(n))
            .filter(|n| !n.is_empty())
            .collect();
        let found = native
            .iter()
            .find(|n| wanted.contains(&normalize(&n.name)))
            .or_else(|| {
                // Wispr renames the Mac's own mic for display.
                r.name
                    .to_lowercase()
                    .starts_with("built-in mic")
                    .then(|| native.iter().find(|n| built_in(n)))
                    .flatten()
            });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// AVFoundation's audio inputs from `ffmpeg -f avfoundation -list_devices
/// true -i ""` (on stderr): "[AVFoundation indev @ 0x…] [1] MacBook Pro
/// Microphone" after the "audio devices:" line.
pub fn parse_avfoundation(listing: &str) -> Vec<Native> {
    let mut audio = false;
    let mut out = Vec::new();
    for line in listing.lines() {
        if line.contains("AVFoundation audio devices") {
            audio = true;
            continue;
        }
        if line.contains("AVFoundation video devices") {
            audio = false;
            continue;
        }
        if !audio {
            continue;
        }
        // "...] [N] Name": the last "] [" starts the index.
        let Some(at) = line.rfind("] [") else {
            continue;
        };
        let rest = &line[at + 3..];
        let Some(close) = rest.find("] ") else {
            continue;
        };
        if rest[..close].parse::<u32>().is_err() {
            continue;
        }
        let name = rest[close + 2..].trim();
        if !name.is_empty() {
            out.push(Native {
                id: name.to_string(),
                name: name.to_string(),
            });
        }
    }
    out
}

#[derive(Deserialize)]
struct PwObject {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    info: Option<PwInfo>,
}

#[derive(Deserialize)]
struct PwInfo {
    #[serde(default)]
    props: serde_json::Map<String, serde_json::Value>,
}

/// PipeWire's capture sources from `pw-dump`: nodes of class Audio/Source,
/// by node.name, described by node.description (Chromium's label).
pub fn parse_pw_dump(json: &str) -> Vec<Native> {
    let objects: Vec<PwObject> = serde_json::from_str(json).unwrap_or_default();
    objects
        .into_iter()
        .filter(|o| o.kind == "PipeWire:Interface:Node")
        .filter_map(|o| {
            let props = o.info?.props;
            let prop = |k: &str| props.get(k).and_then(|v| v.as_str()).map(String::from);
            if prop("media.class")?.as_str() != "Audio/Source" {
                return None;
            }
            let id = prop("node.name")?;
            let name = prop("node.description")
                .or_else(|| prop("node.nick"))
                .unwrap_or_else(|| id.clone());
            Some(Native { id, name })
        })
        .collect()
}

/// How the recorder captures, which decides how inputs are listed and named.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    /// pw-cat: `--target <node.name>`.
    PipeWire,
    /// ffmpeg's avfoundation input: `-i :<name>`.
    AvFoundation,
    /// Anything else (tests): always the given command.
    Fixed,
}

impl Backend {
    pub fn of(capture: &[String]) -> Backend {
        if capture.iter().any(|c| c == "--record") {
            Backend::PipeWire
        } else if capture.iter().any(|c| c == "avfoundation") {
            Backend::AvFoundation
        } else {
            Backend::Fixed
        }
    }
}

/// The system's inputs; `program` is the capture program (pw-cat or ffmpeg).
/// Empty if they cannot be listed.
pub fn list(backend: Backend, program: &str) -> Vec<Native> {
    let output = match backend {
        Backend::PipeWire => {
            // pw-dump sits next to pw-cat.
            let dump = std::path::Path::new(program).with_file_name("pw-dump");
            let dump = if dump.is_absolute() {
                dump.into_os_string()
            } else {
                "pw-dump".into()
            };
            Command::new(dump)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
        }
        Backend::AvFoundation => Command::new(program)
            .args([
                "-hide_banner",
                "-f",
                "avfoundation",
                "-list_devices",
                "true",
                "-i",
                "",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output(),
        Backend::Fixed => return Vec::new(),
    };
    let Ok(output) = output else {
        return Vec::new();
    };
    match backend {
        Backend::PipeWire => parse_pw_dump(&String::from_utf8_lossy(&output.stdout)),
        _ => parse_avfoundation(&String::from_utf8_lossy(&output.stderr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(names: &[&str]) -> Vec<Native> {
        names
            .iter()
            .map(|n| Native {
                id: n.to_string(),
                name: n.to_string(),
            })
            .collect()
    }

    fn ranked(name: &str) -> Ranked {
        Ranked {
            name: name.into(),
            label: None,
            default: false,
        }
    }

    const DEFAULT: Ranked = Ranked {
        name: String::new(),
        label: None,
        default: true,
    };

    #[test]
    fn names_are_reduced_like_wispr_does() {
        assert_eq!(
            normalize("Default - MacBook Pro Microphone (Built-in)"),
            "macbook pro"
        );
        assert_eq!(
            normalize("Jabra Evolve2 65 (0b0e:030c)"),
            "jabra evolve2 65"
        );
        assert_eq!(normalize("Headset (Bluetooth) (2)"), "headset");
        assert_eq!(normalize("  Studio  Microphone "), "studio");
    }

    #[test]
    fn the_first_connected_ranked_mic_wins() {
        let n = native(&["Headset", "MacBook Pro Microphone"]);
        let r = [
            ranked("Yeti (USB)"),
            ranked("Headset"),
            ranked("Built-in mic (recommended)"),
        ];
        assert_eq!(choose(&r, &n).map(|m| m.id.as_str()), Some("Headset"));
    }

    #[test]
    fn default_ranked_first_means_the_system_default() {
        let n = native(&["Headset"]);
        assert_eq!(choose(&[DEFAULT, ranked("Headset")], &n), None);
        assert_eq!(choose(&[], &n), None, "no ranking yet");
        assert_eq!(choose(&[ranked("Gone")], &n), None, "nothing connected");
    }

    #[test]
    fn the_mac_built_in_mic_is_found_by_its_display_name_or_label() {
        let n = native(&["Headset", "MacBook Pro Microphone"]);
        let by_name = [ranked("Built-in mic (recommended)")];
        assert_eq!(
            choose(&by_name, &n).map(|m| m.id.as_str()),
            Some("MacBook Pro Microphone")
        );
        let by_label = [Ranked {
            name: "Built-in mic (recommended)".into(),
            label: Some("MacBook Pro Microphone (Built-in)".into()),
            default: false,
        }];
        assert_eq!(
            choose(&by_label, &n).map(|m| m.id.as_str()),
            Some("MacBook Pro Microphone")
        );
    }

    #[test]
    fn pipewire_sources_match_by_description() {
        let n = vec![Native {
            id: "alsa_input.usb-GN_Netcom_A_S_Jabra_EVOLVE_65-00.mono-fallback".into(),
            name: "Jabra EVOLVE 65 Mono".into(),
        }];
        let r = [Ranked {
            name: "Jabra EVOLVE 65 Mono".into(),
            label: Some("Jabra EVOLVE 65 Mono".into()),
            default: false,
        }];
        assert_eq!(
            choose(&r, &n).map(|m| m.id.as_str()),
            n.first().map(|m| m.id.as_str())
        );
    }

    #[test]
    fn avfoundation_listing_is_parsed() {
        let listing = "\
[AVFoundation indev @ 0xc4f064140] AVFoundation video devices:
[AVFoundation indev @ 0xc4f064140] [0] FaceTime HD Camera
[AVFoundation indev @ 0xc4f064140] AVFoundation audio devices:
[AVFoundation indev @ 0xc4f064140] [0] Headset
[AVFoundation indev @ 0xc4f064140] [1] MacBook Pro Microphone
[in#0 @ 0xc4f064000] Error opening input: Input/output error
";
        assert_eq!(
            parse_avfoundation(listing),
            native(&["Headset", "MacBook Pro Microphone"])
        );
    }

    #[test]
    fn pw_dump_sources_are_parsed() {
        let json = r#"[
          {"id": 30, "type": "PipeWire:Interface:Node", "info": {"props": {
            "media.class": "Audio/Source", "node.name": "alsa_input.pci-0000_00_1f.3.analog-stereo",
            "node.description": "Built-in Audio Analog Stereo"}}},
          {"id": 31, "type": "PipeWire:Interface:Node", "info": {"props": {
            "media.class": "Audio/Sink", "node.name": "alsa_output.x", "node.description": "Speakers"}}},
          {"id": 2, "type": "PipeWire:Interface:Client", "info": {"props": {}}}
        ]"#;
        assert_eq!(
            parse_pw_dump(json),
            vec![Native {
                id: "alsa_input.pci-0000_00_1f.3.analog-stereo".into(),
                name: "Built-in Audio Analog Stereo".into(),
            }]
        );
    }

    #[test]
    fn the_backend_follows_the_capture_command() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(Backend::of(&v("pw-cat --record --raw")), Backend::PipeWire);
        assert_eq!(
            Backend::of(&v("ffmpeg -f avfoundation -i :default")),
            Backend::AvFoundation
        );
        assert_eq!(Backend::of(&v("cat tone.raw")), Backend::Fixed);
    }
}
