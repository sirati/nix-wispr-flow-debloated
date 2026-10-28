//! Messages between the Electron shim and this process: one JSON object per
//! line, on stdin (from the shim) and stdout (to the shim).

use serde::{Deserialize, Serialize};

/// From the shim.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Command {
    /// Wispr asked its Hub to start recording (`dictation:audioStart`).
    Start {
        uuid: String,
    },
    /// `dictation:audioStop`.
    Stop,
    /// Wispr released its processing slot, or entered its retry UI.
    CloudState {
        value: String,
        #[serde(default)]
        id: Option<u64>,
    },
    LocalAlternative {
        id: u64,
    },
    #[serde(skip)]
    AlternativeTranscribed {
        id: u64,
        result: Result<String, String>,
    },
    #[serde(skip)]
    AlternativeDelivered {
        id: u64,
        text: String,
        result: crate::destination::Delivery,
    },
    /// `status:dictationStatus`: initializing, listening, stopping,
    /// processing, retrying, idle, dismissed, error (others are ignored).
    Status {
        value: String,
    },
    /// `status:popo`: the dictation is hands-free.
    Popo,
    /// `status:micSilence`.
    Silence {
        value: bool,
    },
    /// `status:commandModeStatus`.
    Commanding {
        active: bool,
    },
    /// `status:setIndicatorState`; only "resting" reaches us.
    Indicator {
        state: String,
    },
    /// Show a simple notification (Wispr's "condensed" layout): title,
    /// button labels, timeout in milliseconds (0: none).
    Notify {
        title: String,
        actions: Vec<String>,
        timeout: f64,
    },
    /// `notification:clear` from main.
    NotifyClear,
    /// Preserve a completed transcript in the native Copy notice.
    Recover {
        text: String,
        #[serde(default)]
        local: bool,
    },
    /// Ordinary insertion after the shim checked Wispr's editable input info.
    TypeTranscript {
        text: String,
    },
    /// Wispr's microphone ranking (`prefs.user.rankedAudioDevices`).
    Mics {
        ranked: Vec<crate::mics::Ranked>,
    },
    /// The dictation shortcut went down: start recording ahead of Wispr.
    Early,
    /// Another key joined the shortcut before Wispr asked to record: drop
    /// the early recording.
    EarlyCancel,
    /// Internal, never from the shim: the system's inputs were listed again.
    #[serde(skip)]
    Relisted,
    /// A dictation this process starts itself (its own shortcut, or a test):
    /// transcribed by the local model, or handed to Wispr through its bar's
    /// start button. `backend` overrides --backend for this one.
    Dictate {
        #[serde(default)]
        backend: Option<Backend>,
    },
    /// Ends it. A local one's text follows as `transcript`.
    DictateEnd,
    /// Drops it: no text (a local one is not transcribed; Wispr's gets its
    /// bar's cancel button). Also drops a recording started early.
    DictateCancel,
    /// Internal: a local dictation's text, or why there is none.
    #[serde(skip)]
    Transcribed {
        result: Result<(String, crate::destination::Delivery), String>,
        destination: u64,
    },
    #[serde(skip)]
    InsertionFailed {
        text: String,
        error: String,
    },
    /// Final Wispr transcript, intercepted before the helper can paste it.
    CloudTranscript {
        request: String,
        uuid: String,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    #[serde(skip)]
    CloudDelivered {
        request: String,
        result: crate::destination::Delivery,
    },
    #[serde(skip)]
    Destination {
        id: u64,
        icon: Option<std::sync::Arc<crate::icon::Icon>>,
    },
    /// `status:audioLevel`.
    Level {
        value: f64,
    },
    /// Whether this process should draw the speech indicator. Off while
    /// Wispr's own status window is rendering.
    Pill {
        enabled: bool,
    },
    /// Play one of Wispr's sounds: start, stop, paste, achievement, popo.
    Sound {
        name: String,
    },
    /// Whether Wispr shows its resting bar when idle.
    Bar {
        visible: bool,
    },
    /// `status:micActive`.
    Mic {
        active: bool,
    },
    /// Tests (`--hotkey-source stdin`): a key went down or up, as the
    /// keyboard would report it (this platform's code, hotkey/keys.rs).
    Key {
        key: u16,
        down: bool,
    },
    /// Internal: what the dictation shortcut did (control.rs).
    #[serde(skip)]
    Hotkey(Vec<crate::control::Input>),
    /// Internal: a click on the flow bar: start, stop or cancel.
    #[serde(skip)]
    BarClick(String),
    /// Internal: a click on a button of the bar's notice, by index.
    #[serde(skip)]
    NoticeAction {
        index: usize,
    },
    /// Internal: the bar's notice closed: "timeout" or "action".
    #[serde(skip)]
    NoticeClosed {
        reason: String,
    },
}

/// Timing figures Wispr's Hub reports with `dictation:recordingDone`, in
/// milliseconds; field names as Wispr expects them.
#[derive(Debug, Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    pub start_time: f64,
    pub stop_time: f64,
    pub absolute_stop_time: f64,
    pub audio_device_name: String,
    pub mic_acquisition_succeeded: bool,
    pub media_stream_setup: f64,
    pub worklet_setup: f64,
    pub first_audio_chunk_latency: f64,
    pub recording_time: f64,
    pub time_to_receive_last_audio_chunk: f64,
    pub recording_finish: f64,
}

/// To the shim.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Event {
    Ready,
    CloudStart {
        id: u64,
    },
    CloudDelivery {
        request: String,
        fallback: bool,
        applied: bool,
        error: Option<String>,
    },
    Started {
        uuid: String,
    },
    /// One `dictation:audioChunk`: 16-bit little-endian mono PCM at 16 kHz,
    /// base64 encoded.
    Chunk {
        uuid: String,
        index: u64,
        volume: f64,
        duration: f64,
        pcm: String,
    },
    Done {
        uuid: String,
        timing: Timing,
    },
    MicFailed {
        uuid: String,
        error: String,
    },
    /// The bar was clicked: "start" from rest, "stop" while recording (or
    /// hands-free accept), "cancel" for the hands-free cancel button.
    Click {
        action: String,
    },
    /// A notification button was clicked, by index.
    NoticeAction {
        index: usize,
    },
    /// The notification closed by itself: "timeout" or "action".
    NoticeClosed {
        reason: String,
    },
    /// The text of a dictation transcribed here.
    Transcript {
        text: String,
    },
    /// Check ordinary insertion, only after the broker declined the session.
    InsertTranscript {
        text: String,
    },
    /// Copy without typing or restoring the previous clipboard.
    CopyTranscript {
        text: String,
    },
    /// A dictation to the local model failed: no model, no microphone, or
    /// the model's error.
    TranscriptFailed {
        error: String,
    },
    Log {
        msg: String,
    },
    /// Press (`down`) or release Wispr's shortcut key for `action`: "ptt"
    /// (push-to-talk) or "popo" (hands-free). The shim injects it into the
    /// key events of Wispr's helper.
    WisprKey {
        action: String,
        down: bool,
    },
    /// Keys (Wispr's codes) of our shortcuts for Wispr: the shim keeps them
    /// from Wispr, so the keys Wispr sees held match its shortcut exactly.
    HideKeys {
        keys: Vec<u16>,
    },
}

/// Where a dictation's audio goes.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Wispr's service, as for Wispr's own dictations.
    Wispr,
    /// Granite Speech on this machine (asr/).
    Local,
}

impl Backend {
    pub fn parse(name: &str) -> Option<Backend> {
        match name {
            "wispr" => Some(Backend::Wispr),
            "local" => Some(Backend::Local),
            _ => None,
        }
    }
}

pub fn parse(line: &str) -> Result<Command, serde_json::Error> {
    serde_json::from_str(line)
}

pub fn encode(event: &Event) -> String {
    serde_json::to_string(event).expect("events always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(
            parse(r#"{"op":"start","uuid":"u1"}"#).unwrap(),
            Command::Start { uuid: "u1".into() }
        );
        assert_eq!(parse(r#"{"op":"stop"}"#).unwrap(), Command::Stop);
        assert_eq!(
            parse(r#"{"op":"level","value":0.5}"#).unwrap(),
            Command::Level { value: 0.5 }
        );
        assert_eq!(
            parse(r#"{"op":"pill","enabled":false}"#).unwrap(),
            Command::Pill { enabled: false }
        );
        assert_eq!(
            parse(r#"{"op":"bar","visible":true}"#).unwrap(),
            Command::Bar { visible: true }
        );
        // The shim's barCommand() names.
        assert_eq!(parse(r#"{"op":"popo"}"#).unwrap(), Command::Popo);
        assert_eq!(
            parse(r#"{"op":"silence","value":true}"#).unwrap(),
            Command::Silence { value: true }
        );
        assert_eq!(
            parse(r#"{"op":"commanding","active":true}"#).unwrap(),
            Command::Commanding { active: true }
        );
        assert_eq!(
            parse(r#"{"op":"indicator","state":"resting"}"#).unwrap(),
            Command::Indicator {
                state: "resting".into()
            }
        );
        assert!(parse(r#"{"op":"nonsense"}"#).is_err());
        // Internal commands never come from the shim.
        assert!(parse(r#"{"op":"relisted"}"#).is_err());
        assert_eq!(
            parse(r#"{"op":"dictateCancel"}"#).unwrap(),
            Command::DictateCancel
        );
    }

    #[test]
    fn dictations_of_our_own_name_their_backend_or_not() {
        assert_eq!(
            parse(r#"{"op":"dictate"}"#).unwrap(),
            Command::Dictate { backend: None }
        );
        assert_eq!(
            parse(r#"{"op":"dictate","backend":"local"}"#).unwrap(),
            Command::Dictate {
                backend: Some(Backend::Local)
            }
        );
        assert_eq!(
            parse(r#"{"op":"dictateEnd"}"#).unwrap(),
            Command::DictateEnd
        );
        assert!(parse(r#"{"op":"dictate","backend":"cloud"}"#).is_err());
        assert!(parse(r#"{"op":"transcribed"}"#).is_err(), "internal only");
        assert_eq!(
            encode(&Event::TranscriptFailed { error: "x".into() }),
            r#"{"op":"transcriptFailed","error":"x"}"#
        );
    }

    #[test]
    fn chunk_uses_wispr_field_names() {
        let line = encode(&Event::Chunk {
            uuid: "u".into(),
            index: 3,
            volume: -40.0,
            duration: 40.0,
            pcm: "AAA=".into(),
        });
        assert_eq!(
            line,
            r#"{"op":"chunk","uuid":"u","index":3,"volume":-40.0,"duration":40.0,"pcm":"AAA="}"#
        );
    }

    #[test]
    fn timing_uses_wispr_field_names() {
        let t = Timing {
            start_time: 1.0,
            stop_time: 2.0,
            absolute_stop_time: 3.0,
            audio_device_name: "mic".into(),
            mic_acquisition_succeeded: true,
            media_stream_setup: 4.0,
            worklet_setup: 0.0,
            first_audio_chunk_latency: 5.0,
            recording_time: 6.0,
            time_to_receive_last_audio_chunk: 7.0,
            recording_finish: 8.0,
        };
        let v: serde_json::Value = serde_json::to_value(&t).unwrap();
        for k in [
            "startTime",
            "stopTime",
            "absoluteStopTime",
            "audioDeviceName",
            "micAcquisitionSucceeded",
            "mediaStreamSetup",
            "workletSetup",
            "firstAudioChunkLatency",
            "recordingTime",
            "timeToReceiveLastAudioChunk",
            "recordingFinish",
        ] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
    }
}
