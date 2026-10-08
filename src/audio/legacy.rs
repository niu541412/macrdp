//! macOS 10.13 audio input capture for RDPSND.
//!
//! High Sierra has no system-audio capture API. Configure a loopback device
//! as both system output and default input; AudioQueue records that input.

use super::*;
use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    capacity: u32,
    data: *mut c_void,
    data_byte_size: u32,
    user_data: *mut c_void,
    packet_desc_capacity: u32,
    packet_descs: *mut c_void,
    packet_desc_count: u32,
}

type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioQueueNewInput(
        format: *const AudioStreamBasicDescription,
        callback: unsafe extern "C" fn(
            *mut c_void,
            AudioQueueRef,
            AudioQueueBufferRef,
            *const c_void,
            u32,
            *const c_void,
        ),
        user_data: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(
        queue: AudioQueueRef,
        size: u32,
        buffer: *mut AudioQueueBufferRef,
    ) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: AudioQueueBufferRef,
        packet_desc_count: u32,
        packet_descs: *const c_void,
    ) -> i32;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> i32;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> i32;
    fn AudioQueueSetProperty(
        queue: AudioQueueRef,
        property: u32,
        data: *const c_void,
        data_size: u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    fn CFStringGetCString(string: *const c_void, buffer: *mut i8, size: isize, encoding: u32)
        -> u8;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
}

fn address(selector: [u8; 4]) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        selector: u32::from_be_bytes(selector),
        scope: u32::from_be_bytes(*b"glob"),
        element: 0,
    }
}

fn property_u32(object: u32, selector: [u8; 4]) -> Result<u32> {
    let mut value = 0u32;
    let mut size = 4u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address(selector),
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    if status != 0 || size != 4 {
        return Err(anyhow!(
            "CoreAudio property {:?}: OSStatus {status}",
            selector
        ));
    }
    Ok(value)
}

fn property_string(object: u32, selector: [u8; 4]) -> Result<String> {
    let mut value: *const c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const c_void>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address(selector),
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut *const c_void).cast(),
        )
    };
    if status != 0 || value.is_null() {
        return Err(anyhow!(
            "CoreAudio string property {:?}: OSStatus {status}",
            selector
        ));
    }
    let mut bytes = [0i8; 256];
    let ok = unsafe { CFStringGetCString(value, bytes.as_mut_ptr(), 256, 0x0800_0100) };
    if ok == 0 {
        return Err(anyhow!("CoreAudio device property is not UTF-8"));
    }
    Ok(unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

fn default_input_name() -> Result<String> {
    let device = property_u32(1, *b"dIn ")?;
    property_string(device, *b"lnam")
}

fn find_device_uid(requested: &str) -> Result<String> {
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(1, &address(*b"dev#"), 0, std::ptr::null(), &mut size)
    };
    if status != 0 || size == 0 || size % 4 != 0 {
        return Err(anyhow!(
            "CoreAudio device enumeration failed: OSStatus {status}"
        ));
    }
    let mut devices = vec![0u32; (size / 4) as usize];
    let status = unsafe {
        AudioObjectGetPropertyData(
            1,
            &address(*b"dev#"),
            0,
            std::ptr::null(),
            &mut size,
            devices.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return Err(anyhow!(
            "CoreAudio device list read failed: OSStatus {status}"
        ));
    }
    for device in devices {
        if property_string(device, *b"lnam").is_ok_and(|name| name.eq_ignore_ascii_case(requested))
        {
            return property_string(device, *b"uid ");
        }
    }
    Err(anyhow!("CoreAudio device '{requested}' was not found"))
}

fn require_loopback_input(requested: Option<&str>) -> Result<Option<String>> {
    let name = match requested {
        Some(name) => name.to_owned(),
        None => default_input_name()?,
    };
    let lower = name.to_ascii_lowercase();
    if ["blackhole", "soundflower", "loopback", "virtualaudio"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        info!(input_device = %name, "legacy system audio input selected");
        return requested.map(find_device_uid).transpose();
    }
    Err(anyhow!(
        "input '{name}' is not a loopback device; refusing to stream a physical microphone as system sound"
    ))
}

struct CallbackState {
    sender: mpsc::UnboundedSender<Vec<u8>>,
}

unsafe extern "C" fn input_callback(
    user_data: *mut c_void,
    queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
    _start_time: *const c_void,
    _packet_count: u32,
    _packet_descs: *const c_void,
) {
    if !user_data.is_null() && !buffer.is_null() {
        let state = &*(user_data as *const CallbackState);
        let b = &*buffer;
        if !b.data.is_null() && b.data_byte_size > 0 {
            let bytes = std::slice::from_raw_parts(b.data as *const u8, b.data_byte_size as usize);
            let _ = state.sender.send(bytes.to_vec());
        }
        let _ = AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null());
    }
}

struct InputQueue {
    receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputQueue {
    fn new(device_uid: Option<String>) -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("macrdp-legacy-audioqueue".into())
            .spawn(move || {
                let mut state = Box::new(CallbackState { sender });
                let format = AudioStreamBasicDescription {
                    sample_rate: f64::from(SAMPLE_RATE),
                    format_id: 0x6c70_636d,  // kAudioFormatLinearPCM ('lpcm')
                    format_flags: 0x4 | 0x8, // signed integer, packed
                    bytes_per_packet: 4,
                    frames_per_packet: 1,
                    bytes_per_frame: 4,
                    channels_per_frame: 2,
                    bits_per_channel: 16,
                    reserved: 0,
                };
                let mut queue: AudioQueueRef = std::ptr::null_mut();
                let status = unsafe {
                    AudioQueueNewInput(
                        &format,
                        input_callback,
                        (&mut *state as *mut CallbackState).cast(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        &mut queue,
                    )
                };
                if status != 0 {
                    let _ =
                        ready_tx.send(Err(anyhow!("AudioQueueNewInput failed: OSStatus {status}")));
                    return;
                }
                if let Some(uid) = device_uid {
                    let uid = match std::ffi::CString::new(uid) {
                        Ok(uid) => uid,
                        Err(_) => {
                            let _ = ready_tx.send(Err(anyhow!("audio device UID contains NUL")));
                            unsafe { AudioQueueDispose(queue, 1) };
                            return;
                        }
                    };
                    let cf_uid = unsafe {
                        CFStringCreateWithCString(std::ptr::null(), uid.as_ptr(), 0x0800_0100)
                    };
                    if cf_uid.is_null() {
                        let _ = ready_tx.send(Err(anyhow!("cannot create CoreAudio device UID")));
                        unsafe { AudioQueueDispose(queue, 1) };
                        return;
                    }
                    let status = unsafe {
                        AudioQueueSetProperty(
                            queue,
                            u32::from_be_bytes(*b"aqcd"),
                            (&cf_uid as *const *const c_void).cast(),
                            std::mem::size_of::<*const c_void>() as u32,
                        )
                    };
                    unsafe { CFRelease(cf_uid) };
                    if status != 0 {
                        let _ = ready_tx.send(Err(anyhow!(
                            "AudioQueueSetProperty(CurrentDevice) failed: OSStatus {status}"
                        )));
                        unsafe { AudioQueueDispose(queue, 1) };
                        return;
                    }
                }
                let setup = (|| -> Result<()> {
                    for _ in 0..4 {
                        let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
                        let status = unsafe { AudioQueueAllocateBuffer(queue, 4096, &mut buffer) };
                        if status != 0 {
                            return Err(anyhow!("AudioQueueAllocateBuffer: OSStatus {status}"));
                        }
                        let status =
                            unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) };
                        if status != 0 {
                            return Err(anyhow!("AudioQueueEnqueueBuffer: OSStatus {status}"));
                        }
                    }
                    let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
                    if status != 0 {
                        return Err(anyhow!("AudioQueueStart: OSStatus {status}"));
                    }
                    Ok(())
                })();
                let healthy = setup.is_ok();
                let _ = ready_tx.send(setup);
                if healthy {
                    while !thread_stop.load(Ordering::Relaxed) {
                        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 1) };
                    }
                    unsafe { AudioQueueStop(queue, 1) };
                }
                unsafe { AudioQueueDispose(queue, 1) };
            })
            .context("spawn AudioQueue thread")?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("AudioQueue setup timed out")??;
        Ok(Self {
            receiver,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for InputQueue {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn capture_loop(
    sender: Sender,
    audio_sender: AudioSender,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    display_suppressed: Option<Arc<AtomicBool>>,
    mute_on_minimize: bool,
    use_aac: bool,
    aac_bitrate: u32,
    legacy_audio_device: Option<String>,
) -> Result<()> {
    let uid = require_loopback_input(legacy_audio_device.as_deref())?;
    let mut input = InputQueue::new(uid).context(
        "legacy audio needs an active default input device (for system sound, use a loopback device)",
    )?;
    let mut aac = if use_aac {
        Some(crate::aac::AacEncoder::new(
            SAMPLE_RATE,
            CHANNELS,
            aac_bitrate,
        )?)
    } else {
        None
    };
    let started = Instant::now();
    let mut sound_sender: Option<mpsc::Sender<AudioWave>> = None;
    let mut event_sender: Option<mpsc::UnboundedSender<ServerEvent>> = None;
    loop {
        if generation.load(Ordering::SeqCst) != my_gen {
            return Ok(());
        }
        let pcm =
            match tokio::time::timeout(Duration::from_millis(100), input.receiver.recv()).await {
                Ok(Some(pcm)) => pcm,
                Ok(None) => return Err(anyhow!("AudioQueue callback stream stopped")),
                Err(_) => continue,
            };
        if mute_on_minimize
            && display_suppressed
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            continue;
        }
        let waves: Vec<(Vec<u8>, Option<f64>)> = if let Some(enc) = aac.as_mut() {
            let duration = crate::aac::packet_duration_ms(enc.sample_rate());
            enc.encode(&pcm)?
                .into_iter()
                .map(|packet| (packet, Some(duration)))
                .collect()
        } else {
            vec![(pcm, None)]
        };
        for (data, duration) in waves {
            let timestamp = started.elapsed().as_millis() as u32;
            if sound_sender.is_none() {
                sound_sender = audio_sender.lock().unwrap().clone();
            }
            if let Some(output) = &sound_sender {
                match output.try_send((data, timestamp, duration)) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                }
            } else {
                if event_sender.is_none() {
                    event_sender = sender.lock().unwrap().clone();
                }
                if let Some(output) = &event_sender {
                    if output
                        .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                            data, timestamp,
                        )))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
}
