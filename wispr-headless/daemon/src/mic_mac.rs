//! The microphone on macOS, captured in this process with CoreAudio's
//! input unit (AUHAL). The unit is prepared while idle (created, pointed at
//! the device, initialised: the microphone stays off), so a dictation only
//! has to start it: measured on a MacBook, 8 ms to start and 25 ms to the
//! first audio, against 35-120 ms plus configuration for AVCaptureSession
//! and 230-310 ms for spawning ffmpeg. Audio arrives as 32-bit float at the
//! device's rate; resample.rs makes Wispr's 16 kHz 16-bit of it.
//!
//! CoreAudio is a C API, so this module allows unsafe code. Each block says
//! why the call is sound.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitGetProperty, AudioUnitInitialize, AudioUnitRender,
    AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFRetained, CFString};

use crate::mics::Native;
use crate::resample::Resampler;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

// CoreAudio's constants (AudioHardware.h, AudioUnitProperties.h).
const SYSTEM_OBJECT: AudioObjectID = 1;
const SCOPE_GLOBAL_OBJ: u32 = fourcc(b"glob");
const SCOPE_INPUT_OBJ: u32 = fourcc(b"inpt");
const DEVICES: u32 = fourcc(b"dev#");
const DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const NAME: u32 = fourcc(b"lnam");
const STREAMS: u32 = fourcc(b"stm#");
const ENABLE_IO: u32 = 2003;
const CURRENT_DEVICE: u32 = 2000;
const SET_INPUT_CALLBACK: u32 = 2005;
const STREAM_FORMAT: u32 = 8;
const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;
/// AUHAL's input element.
const INPUT_BUS: u32 = 1;
const LINEAR_PCM: u32 = fourcc(b"lpcm");
/// kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked.
const FLOAT_PACKED: u32 = 1 | 8;

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: 0,
    }
}

fn check(status: i32, what: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

/// All audio devices.
fn devices() -> Vec<AudioObjectID> {
    let mut addr = address(DEVICES, SCOPE_GLOBAL_OBJ);
    let mut size = 0u32;
    // SAFETY: asks the system object for the size of its device list.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            SYSTEM_OBJECT,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let mut ids = vec![0 as AudioObjectID; size as usize / std::mem::size_of::<AudioObjectID>()];
    // SAFETY: the buffer holds exactly `size` bytes of device IDs.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr()).cast(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    ids
}

/// Whether a device can record (has input streams).
fn has_input(id: AudioObjectID) -> bool {
    let mut addr = address(STREAMS, SCOPE_INPUT_OBJ);
    let mut size = 0u32;
    // SAFETY: a size query on a device ID from the system's list.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            id,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    status == 0 && size > 0
}

fn name(id: AudioObjectID) -> Option<String> {
    let mut addr = address(NAME, SCOPE_GLOBAL_OBJ);
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    let mut raw: *const CFString = std::ptr::null();
    // SAFETY: kAudioObjectPropertyName yields one CFStringRef, which the
    // caller owns.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut raw).cast(),
        )
    };
    if status != 0 {
        return None;
    }
    let raw = NonNull::new(raw.cast_mut())?;
    // SAFETY: we own the returned reference (a Copy rule property).
    let name = unsafe { CFRetained::from_raw(raw) };
    Some(name.to_string())
}

/// The system's microphones by name (the names AVFoundation and ffmpeg use).
pub fn inputs() -> Vec<Native> {
    devices()
        .into_iter()
        .filter(|&id| has_input(id))
        .filter_map(name)
        .map(|n| Native {
            id: n.clone(),
            name: n,
        })
        .collect()
}

fn default_input() -> Option<AudioObjectID> {
    let mut addr = address(DEFAULT_INPUT, SCOPE_GLOBAL_OBJ);
    let mut id: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: the default input device is one AudioObjectID.
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut id).cast(),
        )
    };
    (status == 0 && id != 0).then_some(id)
}

/// The device called `name` that can record, or the default input.
fn device(name: Option<&str>) -> Option<AudioObjectID> {
    match name {
        Some(n) => devices()
            .into_iter()
            .find(|&id| has_input(id) && self::name(id).as_deref() == Some(n)),
        None => default_input(),
    }
}

/// What the input callback works with.
struct Shared {
    unit: AudioUnit,
    /// Where audio goes while recording; None while prepared.
    tx: Mutex<Option<Sender<Vec<u8>>>>,
    resampler: Mutex<Resampler>,
    scratch: Mutex<Vec<f32>>,
}

/// The input callback: renders the device's new frames and sends them on.
unsafe extern "C-unwind" fn on_input(
    refcon: NonNull<c_void>,
    flags: NonNull<AudioUnitRenderActionFlags>,
    time: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    _data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: refcon is the Shared this unit was set up with; it outlives
    // the unit (Prepared disposes the unit before dropping it).
    let shared = unsafe { refcon.cast::<Shared>().as_ref() };
    let Ok(mut scratch) = shared.scratch.lock() else {
        return 0;
    };
    scratch.resize(frames as usize, 0.0);
    let mut list = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: frames * 4,
            mData: scratch.as_mut_ptr().cast(),
        }],
    };
    // SAFETY: renders `frames` mono float frames into a buffer of that size,
    // with the flags and time stamp CoreAudio gave this callback.
    let status = unsafe {
        AudioUnitRender(
            shared.unit,
            flags.as_ptr(),
            time,
            bus,
            frames,
            NonNull::from(&mut list),
        )
    };
    if status != 0 {
        return status;
    }
    let tx = shared.tx.lock().ok().and_then(|t| t.clone());
    if let (Some(tx), Ok(mut resampler)) = (tx, shared.resampler.lock()) {
        let pcm = resampler.push(&scratch);
        if !pcm.is_empty() {
            let _ = tx.send(pcm);
        }
    }
    0
}

/// An input unit set up for one microphone, ready to start.
pub struct Prepared {
    /// The input it was prepared for (None: the system default).
    pub input: Option<String>,
    shared: Box<Shared>,
}

// CoreAudio allows starting and stopping a unit from any thread; the rest of
// Shared is behind mutexes.
unsafe impl Send for Prepared {}
unsafe impl Sync for Prepared {}

impl Prepared {
    /// Sets up the unit for `input` (or the default input) without starting
    /// the microphone.
    pub fn new(input: Option<&str>) -> Result<Prepared, String> {
        let dev = device(input).ok_or_else(|| "no such microphone".to_string())?;
        let mut desc = AudioComponentDescription {
            componentType: fourcc(b"auou"),
            componentSubType: fourcc(b"ahal"),
            componentManufacturer: fourcc(b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: looks up Apple's HAL output unit, which always exists.
        let component =
            unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&mut desc)) };
        if component.is_null() {
            return Err("no HAL audio unit".into());
        }
        let mut unit: AudioUnit = std::ptr::null_mut();
        // SAFETY: creates an instance of that component into `unit`.
        check(
            unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) },
            "creating the audio unit",
        )?;
        let shared = Box::new(Shared {
            unit,
            tx: Mutex::new(None),
            resampler: Mutex::new(Resampler::new(48_000.0, 16_000.0)),
            scratch: Mutex::new(Vec::new()),
        });
        let prepared = Prepared {
            input: input.map(String::from),
            shared,
        };
        prepared.configure(dev)?;
        Ok(prepared)
    }

    fn set<T>(
        &self,
        id: u32,
        scope: u32,
        element: u32,
        value: &T,
        what: &str,
    ) -> Result<(), String> {
        // SAFETY: passes a pointer to `value` with its exact size.
        let status = unsafe {
            AudioUnitSetProperty(
                self.shared.unit,
                id,
                scope,
                element,
                (value as *const T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        check(status, what)
    }

    fn configure(&self, dev: AudioObjectID) -> Result<(), String> {
        self.set(ENABLE_IO, SCOPE_INPUT, INPUT_BUS, &1u32, "enabling input")?;
        self.set(ENABLE_IO, SCOPE_OUTPUT, 0, &0u32, "disabling output")?;
        self.set(CURRENT_DEVICE, SCOPE_GLOBAL, 0, &dev, "choosing the device")?;
        // The device's own format, for its sample rate.
        // SAFETY: a plain C struct of numbers; all-zero is valid.
        let mut device_format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: reads one AudioStreamBasicDescription into a struct of that size.
        check(
            unsafe {
                AudioUnitGetProperty(
                    self.shared.unit,
                    STREAM_FORMAT,
                    SCOPE_INPUT,
                    INPUT_BUS,
                    NonNull::from(&mut device_format).cast(),
                    NonNull::from(&mut size),
                )
            },
            "reading the device format",
        )?;
        let rate = if device_format.mSampleRate > 0.0 {
            device_format.mSampleRate
        } else {
            48_000.0
        };
        // What we take: mono 32-bit float at the device's rate (AUHAL does
        // not change the rate of input; resample.rs does).
        let ours = AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: LINEAR_PCM,
            mFormatFlags: FLOAT_PACKED,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: 1,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        self.set(
            STREAM_FORMAT,
            SCOPE_OUTPUT,
            INPUT_BUS,
            &ours,
            "setting the format",
        )?;
        *self
            .shared
            .resampler
            .lock()
            .expect("resampler lock poisoned") = Resampler::new(rate, 16_000.0);
        let callback = AURenderCallbackStruct {
            inputProc: Some(on_input),
            inputProcRefCon: (&*self.shared as *const Shared).cast_mut().cast(),
        };
        self.set(
            SET_INPUT_CALLBACK,
            SCOPE_GLOBAL,
            0,
            &callback,
            "setting the callback",
        )?;
        // SAFETY: initialises the configured unit; this does not start the
        // microphone.
        check(
            unsafe { AudioUnitInitialize(self.shared.unit) },
            "initialising",
        )
    }

    /// Starts the microphone; audio goes to `tx` as 16 kHz s16 LE PCM.
    pub fn start(&self, tx: Sender<Vec<u8>>) -> Result<(), String> {
        self.shared
            .resampler
            .lock()
            .expect("resampler lock poisoned")
            .reset();
        *self.shared.tx.lock().expect("tx lock poisoned") = Some(tx);
        // SAFETY: starts an initialised unit.
        check(
            unsafe { AudioOutputUnitStart(self.shared.unit) },
            "starting the microphone",
        )
    }

    /// Stops the microphone; the channel closes. The unit stays prepared.
    pub fn stop(&self) {
        // SAFETY: stopping a unit is allowed whether or not it runs.
        unsafe { AudioOutputUnitStop(self.shared.unit) };
        self.shared.tx.lock().expect("tx lock poisoned").take();
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        self.stop();
        // SAFETY: the unit is ours; after disposal no callback runs, so the
        // Shared it points to can go.
        unsafe {
            AudioUnitUninitialize(self.shared.unit);
            AudioComponentInstanceDispose(self.shared.unit);
        }
    }
}
