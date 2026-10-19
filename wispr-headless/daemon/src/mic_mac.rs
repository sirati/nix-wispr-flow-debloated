//! The microphone on macOS, captured in this process: an AVCaptureSession
//! whose audio data output converts to Wispr's format (16 kHz mono 16-bit
//! PCM), so a dictation starts without spawning a recorder.
//!
//! objc2 marks every AVFoundation and CoreMedia call `unsafe` until it has
//! been reviewed upstream, so this module allows unsafe code. Each block says
//! why the call is sound; most are plain method calls with object arguments.
#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection,
    AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession,
};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::mics::Native;

/// AVMediaTypeAudio's value. (The constant is an extern static, which Rust
/// cannot read without unsafe; the string is part of Apple's ABI.)
fn audio() -> Retained<NSString> {
    NSString::from_str("soun")
}

/// The system's microphones by name, as AVFoundation (and ffmpeg) call them.
pub fn inputs() -> Vec<Native> {
    // SAFETY: a class method taking a media type string.
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(&audio()) };
    devices
        .iter()
        .map(|d| {
            // SAFETY: property getters on a live device.
            let name = unsafe { d.localizedName() }.to_string();
            Native {
                id: name.clone(),
                name,
            }
        })
        .collect()
}

struct SinkIvars {
    audio: Mutex<Option<Sender<Vec<u8>>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivars are Send
    // and Sync, as the callback runs on the capture queue.
    #[unsafe(super(NSObject))]
    #[ivars = SinkIvars]
    struct Sink;

    unsafe impl NSObjectProtocol for Sink {}

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for Sink {
        // SAFETY: the signature of the protocol's method.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            // SAFETY: CMSampleBufferGetDataBuffer on the buffer we are given.
            let Some(block) = (unsafe { buffer.data_buffer() }) else {
                return;
            };
            // SAFETY: CMBlockBufferGetDataLength on a live block buffer.
            let len = unsafe { block.data_length() };
            if len == 0 {
                return;
            }
            let mut bytes = vec![0u8; len];
            let Some(dest) = NonNull::new(bytes.as_mut_ptr()) else {
                return;
            };
            // SAFETY: copies exactly `len` bytes, the buffer's length, into a
            // Vec of that length.
            let status = unsafe { block.copy_data_bytes(0, len, dest.cast()) };
            if status != 0 {
                return;
            }
            if let Some(tx) = self
                .ivars()
                .audio
                .lock()
                .expect("sink lock poisoned")
                .as_ref()
            {
                let _ = tx.send(bytes);
            }
        }
    }
);

impl Sink {
    fn new(tx: Sender<Vec<u8>>) -> Retained<Sink> {
        let this = Sink::alloc().set_ivars(SinkIvars {
            audio: Mutex::new(Some(tx)),
        });
        // SAFETY: NSObject's init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// A running capture; stops when dropped.
pub struct Capture {
    session: Retained<AVCaptureSession>,
    sink: Retained<Sink>,
    _queue: DispatchRetained<DispatchQueue>,
}

// The session and sink are only used to stop the capture, which AVFoundation
// allows from any thread.
unsafe impl Send for Capture {}

impl Capture {
    /// Starts capturing the microphone called `name` (or the system default)
    /// into `tx`, as 16 kHz mono s16 little-endian PCM.
    pub fn start(name: Option<&str>, tx: Sender<Vec<u8>>) -> Result<Capture, String> {
        let media = audio();
        let device = match name {
            // SAFETY: class methods and getters with valid arguments.
            Some(n) => unsafe { AVCaptureDevice::devicesWithMediaType(&media) }
                .iter()
                .find(|d| unsafe { d.localizedName() }.to_string() == n),
            None => unsafe { AVCaptureDevice::defaultDeviceWithMediaType(&media) },
        }
        .ok_or_else(|| format!("no microphone {}", name.unwrap_or("(default)")))?;
        // SAFETY: opens the device; failure is returned as an NSError.
        let input = unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
            .map_err(|e| e.localizedDescription().to_string())?;

        let session = unsafe { AVCaptureSession::new() };
        let output = unsafe { AVCaptureAudioDataOutput::new() };
        // Wispr's format: AVFormatIDKey = kAudioFormatLinearPCM ('lpcm') etc.
        // The keys are AVFoundation's string constants, by value.
        let keys = [
            "AVFormatIDKey",
            "AVSampleRateKey",
            "AVNumberOfChannelsKey",
            "AVLinearPCMBitDepthKey",
            "AVLinearPCMIsFloatKey",
            "AVLinearPCMIsBigEndianKey",
            "AVLinearPCMIsNonInterleaved",
        ]
        .map(NSString::from_str);
        let values = [
            NSNumber::new_u32(u32::from_be_bytes(*b"lpcm")),
            NSNumber::new_f64(16_000.0),
            NSNumber::new_u32(1),
            NSNumber::new_u32(16),
            NSNumber::new_bool(false),
            NSNumber::new_bool(false),
            NSNumber::new_bool(false),
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let value_refs: Vec<&AnyObject> = values.iter().map(|v| v.as_ref()).collect();
        let settings = NSDictionary::from_slices(&key_refs, &value_refs);
        // SAFETY: the settings dictionary has NSString keys and NSNumber
        // values, as setAudioSettings: expects.
        unsafe { output.setAudioSettings(Some(&settings)) };

        let queue = DispatchQueue::new("wispr-headless.microphone", DispatchQueueAttr::SERIAL);
        let sink = Sink::new(tx);
        // SAFETY: the sink implements the delegate protocol and lives as long
        // as this Capture; the queue is serial and also kept here.
        unsafe {
            output.setSampleBufferDelegate_queue(
                Some(ProtocolObject::from_ref(&*sink)),
                Some(&queue),
            );
        }
        // SAFETY: configuring a session we own with an input and output it
        // accepts (checked first).
        unsafe {
            session.beginConfiguration();
            if !session.canAddInput(&input) || !session.canAddOutput(&output) {
                session.commitConfiguration();
                return Err("the microphone cannot be captured".into());
            }
            session.addInput(&input);
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
        }
        Ok(Capture {
            session,
            sink,
            _queue: queue,
        })
    }

    /// Stops capturing; the channel closes once queued audio is delivered.
    pub fn stop(&mut self) {
        // SAFETY: stopping a session we own.
        unsafe { self.session.stopRunning() };
        // No more audio after this: the receiver sees the channel close.
        self.sink
            .ivars()
            .audio
            .lock()
            .expect("sink lock poisoned")
            .take();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}
