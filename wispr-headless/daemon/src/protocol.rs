//! Messages between the Electron shim and this process: one JSON object per
//! line, on stdin (from the shim) and stdout (to the shim).

use serde::{Deserialize, Serialize};

/// From the shim.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Command {
    /// Wispr asked its Hub to start recording (`dictation:audioStart`).
    Start { uuid: String },
    /// `dictation:audioStop`.
    Stop,
    /// `status:dictationStatus`: idle, recording, reconnecting, flushing,
    /// compressing, or anything newer.
    Status { value: String },
    /// `status:audioLevel`.
    Level { value: f64 },
    /// Whether this process should draw the speech indicator. Off while
    /// Wispr's own status window is rendering.
    Pill { enabled: bool },
    /// Play one of Wispr's sounds: start, stop, paste, achievement.
    Sound { name: String },
    /// Whether Wispr shows its resting bar when idle.
    Bar { visible: bool },
    /// `status:micActive`.
    Mic { active: bool },
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
    /// The bar was clicked: "start" from rest, "stop" while recording.
    Click {
        action: String,
    },
    Log {
        msg: String,
    },
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
        assert!(parse(r#"{"op":"nonsense"}"#).is_err());
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
