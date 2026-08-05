// Shared mixer core with per-OS leaf output threads.
// @Todo: Ogg decode (lewton) + replace the rodio path in lib.rs
// @Todo: Linux backend (dlopen libasound.so.2)
// @Todo: Mac backend (CoreAudio)

const PRINT_AUDIO:        bool = 1 == 1;
const PRINT_AUDIO_TIMING: bool = 0 == 1; // @Debug: Wall-clock trace of voice add/retire and underruns

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static AUDIO_EPOCH: OnceLock<Instant> = OnceLock::new();
fn tms() -> u128 { AUDIO_EPOCH.get_or_init(Instant::now).elapsed().as_millis() }

const VOICES_MAX: usize = 64; // Oldest gets dropped; also bounds pile-up when no backend ever comes up

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sound {
    pub rate:   u32,
    pub frames: Vec<[f32; 2]>,
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
struct Voice {
    sound_i: usize,
    cursor:  f64, // Fractional frame index into Sound.frames
    speed:   f32,
    vol:     f32,
}

struct AudioState {
    sounds: Vec<Sound>, // @Todo: Keyed cache; grows on every play
    voices: Vec<Voice>,
    master: f32,
    device_rate: u32, // Zero until a backend is up
}

static AUDIO: Mutex<AudioState> = Mutex::new(AudioState {
    sounds: Vec::new(),
    voices: Vec::new(),
    master: 1.0,
    device_rate: 0,
});
static AUDIO_STARTED: AtomicBool = AtomicBool::new(false);

pub fn setup_audio() {
    if AUDIO_STARTED.swap(true, Ordering::Relaxed) { return; }
    let spawned = std::thread::Builder::new().name("audio".into()).spawn(audio_thread_main);
    if spawned.is_err() && PRINT_AUDIO { eprintln!("audio: couldn't spawn thread"); }
}

// @Todo: Perceptual volume. Perceived loudness goes roughly with amplitude squared, so an
// amplitude of 0.316 sounds about half as loud, not 0.5. Map volume knobs through a curve.
pub fn set_master_volume(vol: f32) { AUDIO.lock().unwrap().master = vol; }

pub fn device_ready() -> bool { AUDIO.lock().unwrap().device_rate != 0 }

pub fn play_sound_pcm(sound: Sound, vol: f32, speed: f32) {
    let audio = &mut *AUDIO.lock().unwrap();
    if audio.device_rate == 0 { return; } // No backend, drop rather than queue a stale burst
    if PRINT_AUDIO_TIMING { eprintln!("[{}ms] add voice: {} frames at {}hz speed {}", tms(), sound.frames.len(), sound.rate, speed); }
    audio.sounds.push(sound);
    let sound_i = audio.sounds.len() - 1;
    if audio.voices.len() >= VOICES_MAX { audio.voices.remove(0); }
    audio.voices.push(Voice { sound_i, cursor: 0.0, speed, vol });
}

pub fn play_sine(hz: f32, secs: f32, vol: f32, speed: f32) {
    let rate = 48000u32;
    let frames_n = (secs * rate as f32) as usize;
    let fade_n   = usize::min(200, frames_n / 2); // Dodges start/end clicks
    let mut frames = Vec::with_capacity(frames_n);
    for i in 0..frames_n {
        let mut s = (i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin();
        if i < fade_n            { s *= i as f32 / fade_n as f32; }
        if i >= frames_n - fade_n { s *= (frames_n - i) as f32 / fade_n as f32; }
        frames.push([s, s]);
    }
    play_sound_pcm(Sound { rate, frames }, vol, speed);
}

// Interleaved f32 out at whatever channel count the device wants; sounds are stereo, extra channels get 0
fn mix_into(buf: &mut [f32], ch_n: usize) {
    for s in buf.iter_mut() { *s = 0.0; }

    let audio = &mut *AUDIO.lock().unwrap();
    let dev_rate = audio.device_rate as f64;
    if dev_rate == 0.0 { return; }
    let master = audio.master;

    let out_frames_n = buf.len() / ch_n;
    let AudioState { sounds, voices, .. } = audio; // Split borrow
    voices.retain_mut(|v| {
        let sound = &sounds[v.sound_i];
        let step = sound.rate as f64 / dev_rate * v.speed as f64;
        let vol  = v.vol * master;
        for out_i in 0..out_frames_n {
            let i = v.cursor as usize;
            if i + 1 >= sound.frames.len() {
                if PRINT_AUDIO_TIMING { eprintln!("[{}ms] retire voice: sound_i {}", tms(), v.sound_i); }
                return false;
            }
            let t = (v.cursor - i as f64) as f32;
            let a = sound.frames[i];
            let b = sound.frames[i + 1];
            buf[out_i*ch_n] += (a[0] + (b[0] - a[0])*t) * vol;
            if ch_n > 1 { buf[out_i*ch_n + 1] += (a[1] + (b[1] - a[1])*t) * vol; }
            v.cursor += step;
        }
        true
    });
}

#[cfg(not(target_os = "windows"))]
fn audio_thread_main() {
    // @Todo: ALSA via dlopen, CoreAudio
    if PRINT_AUDIO { eprintln!("audio: no backend for this OS yet"); }
}

#[cfg(target_os = "windows")]
fn audio_thread_main() { wasapi::thread_main() }

#[cfg(target_os = "windows")]
mod wasapi {
    #![allow(non_snake_case, non_upper_case_globals)]
    // Hand-bound COM. Any failed HRESULT soft-fails the whole backend: no sound, no crash.

    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use super::{mix_into, AUDIO, PRINT_AUDIO};

    type HRESULT = i32;

    #[repr(C)] #[derive(Clone, Copy, PartialEq)]
    struct Guid { a: u32, b: u16, c: u16, d: [u8; 8] }

    const CLSID_MMDeviceEnumerator:       Guid = Guid { a: 0xBCDE0395, b: 0xE52F, c: 0x467C, d: [0x8E,0x3D,0xC4,0x57,0x92,0x91,0x69,0x2E] };
    const IID_IMMDeviceEnumerator:        Guid = Guid { a: 0xA95664D2, b: 0x9614, c: 0x4F35, d: [0xA7,0x46,0xDE,0x8D,0xB6,0x36,0x17,0xE6] };
    const IID_IMMNotificationClient:      Guid = Guid { a: 0x7991EEC9, b: 0x7E89, c: 0x4D85, d: [0x83,0x90,0x6C,0x70,0x3C,0xEC,0x60,0xC0] };
    const IID_IUnknown:                   Guid = Guid { a: 0x00000000, b: 0x0000, c: 0x0000, d: [0xC0,0x00,0x00,0x00,0x00,0x00,0x00,0x46] };
    const IID_IAudioClient:               Guid = Guid { a: 0x1CB9AD4C, b: 0xDBFA, c: 0x4C32, d: [0xB1,0x78,0xC2,0xF5,0x68,0xA7,0x03,0xB2] };
    const IID_IAudioRenderClient:         Guid = Guid { a: 0xF294ACFC, b: 0x3146, c: 0x4483, d: [0xA7,0xBF,0xAD,0xDC,0xA7,0xC2,0x60,0xE2] };
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid { a: 0x00000003, b: 0x0000, c: 0x0010, d: [0x80,0x00,0x00,0xAA,0x00,0x38,0x9B,0x71] };

    const COINIT_MULTITHREADED: u32 = 0;
    const CLSCTX_ALL:           u32 = 0x17;
    const eRender:              u32 = 0;
    const eConsole:             u32 = 0;
    const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
    const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x00040000;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const E_NOINTERFACE: HRESULT = 0x80004002u32 as HRESULT;
    const BUFFER_DUR_100NS: i64 = 20 * 10_000; // 20ms, two engine quanta

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> HRESULT;
        fn CoCreateInstance(clsid: *const Guid, outer: *mut c_void, clsctx: u32, iid: *const Guid, out: *mut *mut c_void) -> HRESULT;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(attrs: *mut c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, timeout_ms: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[repr(C, packed(2))]
    struct WaveFormatEx {
        tag:         u16,
        ch_n:        u16,
        rate:        u32,
        avg_bytes:   u32,
        block_align: u16,
        bits:        u16,
        cb_size:     u16,
        // Extensible tail if tag == WAVE_FORMAT_EXTENSIBLE:
        // valid_bits: u16, channel_mask: u32, sub_format: Guid
    }

    // A COM method call is just an indexed load from the object's vtable, so these structs
    // must list every slot in exactly the order the SDK headers (mmdeviceapi.h, audioclient.h)
    // declare them: QueryInterface/AddRef/Release always occupy the first three slots, and a
    // missing or reordered entry here would silently call some unrelated function. Slots we
    // never call are usize placeholders, and trailing slots past the last one we call are
    // omitted entirely.
    #[repr(C)] struct IUnknownVtbl {
        QueryInterface: usize,
        AddRef:         usize,
        Release:        unsafe extern "system" fn(*mut c_void) -> u32,
    }
    #[repr(C)] struct MMDeviceEnumeratorVtbl {
        _iunknown: [usize; 3],
        EnumAudioEndpoints: usize,
        GetDefaultAudioEndpoint: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT,
        GetDevice: usize,
        RegisterEndpointNotificationCallback: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    }
    #[repr(C)] struct MMDeviceVtbl {
        _iunknown: [usize; 3],
        Activate: unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> HRESULT,
    }
    #[repr(C)] struct AudioClientVtbl {
        _iunknown: [usize; 3],
        Initialize:        unsafe extern "system" fn(*mut c_void, u32, u32, i64, i64, *const WaveFormatEx, *const Guid) -> HRESULT,
        GetBufferSize:     unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        GetStreamLatency:  usize,
        GetCurrentPadding: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        IsFormatSupported: usize,
        GetMixFormat:      unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> HRESULT,
        GetDevicePeriod:   usize,
        Start:             unsafe extern "system" fn(*mut c_void) -> HRESULT,
        Stop:              unsafe extern "system" fn(*mut c_void) -> HRESULT,
        Reset:             usize,
        SetEventHandle:    unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
        GetService:        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HRESULT,
    }
    #[repr(C)] struct AudioRenderClientVtbl {
        _iunknown: [usize; 3],
        GetBuffer:     unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT,
        ReleaseBuffer: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    }

    macro_rules! try_hr {
        ($hr:expr, $what:expr) => { try_hr!($hr, $what, ()) };
        ($hr:expr, $what:expr, $ret:expr) => {
            let hr = $hr;
            if hr < 0 { if PRINT_AUDIO { eprintln!("audio: {} failed (hr={:#010x})", $what, hr as u32); } return $ret; }
        };
    }

    unsafe fn vt<T>(com: *mut c_void) -> &'static T { &**(com as *mut *const T) }
    unsafe fn release(com: *mut c_void) { if !com.is_null() { (vt::<IUnknownVtbl>(com).Release)(com); } }

    // Default-device changes arrive by callback, not polling: IMMNotificationClient is a COM
    // interface that WE implement, so this is COM binding in reverse. The OS calls through this
    // vtable from its own thread, so the methods only touch atomics. The object is a static with
    // a fake refcount because it lives for the whole process. Note the x86_64 ABI passes structs
    // wider than 8 bytes by pointer, which is why every ignored argument can be typed as usize.
    static DEFAULT_CHANGED: AtomicBool = AtomicBool::new(false);

    #[repr(C)] struct NotificationClientVtbl {
        QueryInterface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HRESULT,
        AddRef:         unsafe extern "system" fn(*mut c_void) -> u32,
        Release:        unsafe extern "system" fn(*mut c_void) -> u32,
        OnDeviceStateChanged:   unsafe extern "system" fn(*mut c_void, usize, usize) -> HRESULT,
        OnDeviceAdded:          unsafe extern "system" fn(*mut c_void, usize) -> HRESULT,
        OnDeviceRemoved:        unsafe extern "system" fn(*mut c_void, usize) -> HRESULT,
        OnDefaultDeviceChanged: unsafe extern "system" fn(*mut c_void, u32, u32, usize) -> HRESULT,
        OnPropertyValueChanged: unsafe extern "system" fn(*mut c_void, usize, usize) -> HRESULT,
    }
    unsafe extern "system" fn notify_qi(this: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
        if *iid == IID_IUnknown || *iid == IID_IMMNotificationClient { *out = this; 0 }
        else { *out = std::ptr::null_mut(); E_NOINTERFACE }
    }
    unsafe extern "system" fn notify_ref(_this: *mut c_void) -> u32 { 1 }
    unsafe extern "system" fn notify_nop1(_this: *mut c_void, _a: usize) -> HRESULT { 0 }
    unsafe extern "system" fn notify_nop2(_this: *mut c_void, _a: usize, _b: usize) -> HRESULT { 0 }
    unsafe extern "system" fn notify_default_changed(_this: *mut c_void, flow: u32, role: u32, _id: usize) -> HRESULT {
        if flow == eRender && role == eConsole { DEFAULT_CHANGED.store(true, Ordering::Relaxed); }
        0
    }
    static NOTIFY_VTBL: NotificationClientVtbl = NotificationClientVtbl {
        QueryInterface:         notify_qi,
        AddRef:                 notify_ref,
        Release:                notify_ref,
        OnDeviceStateChanged:   notify_nop2,
        OnDeviceAdded:          notify_nop1,
        OnDeviceRemoved:        notify_nop1,
        OnDefaultDeviceChanged: notify_default_changed,
        OnPropertyValueChanged: notify_nop2,
    };
    #[repr(transparent)] struct NotifyPtr(*const NotificationClientVtbl);
    unsafe impl Sync for NotifyPtr {}
    static NOTIFY: NotifyPtr = NotifyPtr(&NOTIFY_VTBL);

    pub fn thread_main() { unsafe {
        try_hr!(CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED), "CoInitializeEx");

        let mut enumr: *mut c_void = std::ptr::null_mut();
        try_hr!(CoCreateInstance(&CLSID_MMDeviceEnumerator, std::ptr::null_mut(), CLSCTX_ALL, &IID_IMMDeviceEnumerator, &mut enumr), "CoCreateInstance(MMDeviceEnumerator)");

        try_hr!((vt::<MMDeviceEnumeratorVtbl>(enumr).RegisterEndpointNotificationCallback)(enumr, &NOTIFY as *const NotifyPtr as *mut c_void), "RegisterEndpointNotificationCallback");

        loop {
            let ran = run_device(enumr);
            AUDIO.lock().unwrap().device_rate = 0;
            if !ran {
                // No usable output device right now; retry until one appears
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    } }

    // One device session: open the current default endpoint, render until the device dies or the
    // default changes, then tear down so the caller reopens. False = no session could start at all.
    unsafe fn run_device(enumr: *mut c_void) -> bool {
        DEFAULT_CHANGED.store(false, Ordering::Relaxed); // Any change after this point causes a reopen

        let mut device: *mut c_void = std::ptr::null_mut();
        try_hr!((vt::<MMDeviceEnumeratorVtbl>(enumr).GetDefaultAudioEndpoint)(enumr, eRender, eConsole, &mut device), "GetDefaultAudioEndpoint", false);

        let mut client: *mut c_void = std::ptr::null_mut();
        let mut render: *mut c_void = std::ptr::null_mut();
        let mut event:  *mut c_void = std::ptr::null_mut();
        let ran = run_device_session(device, &mut client, &mut render, &mut event);

        if !client.is_null() { let _ = (vt::<AudioClientVtbl>(client).Stop)(client); }
        release(render);
        release(client);
        release(device);
        if !event.is_null() { CloseHandle(event); }
        ran
    }

    unsafe fn run_device_session(device: *mut c_void, client_out: &mut *mut c_void, render_out: &mut *mut c_void, event_out: &mut *mut c_void) -> bool {
        let mut client: *mut c_void = std::ptr::null_mut();
        try_hr!((vt::<MMDeviceVtbl>(device).Activate)(device, &IID_IAudioClient, CLSCTX_ALL, std::ptr::null_mut(), &mut client), "Activate(IAudioClient)", false);
        *client_out = client;

        let mut fmt_ptr: *mut WaveFormatEx = std::ptr::null_mut();
        try_hr!((vt::<AudioClientVtbl>(client).GetMixFormat)(client, &mut fmt_ptr), "GetMixFormat", false);
        // Leaked on purpose; one small alloc per device open

        let tag   = (*fmt_ptr).tag;
        let ch_n  = (*fmt_ptr).ch_n as usize;
        let rate  = (*fmt_ptr).rate;
        let bits  = (*fmt_ptr).bits;
        let is_float = tag == WAVE_FORMAT_IEEE_FLOAT
                    || (tag == WAVE_FORMAT_EXTENSIBLE
                        && *((fmt_ptr as *const u8).add(24) as *const Guid) == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        if !is_float || bits != 32 { // Shared-mode mix format is f32 on everything since Vista; not worth a converter
            if PRINT_AUDIO { eprintln!("audio: mix format not f32 (tag={tag:#x} bits={bits}), no sound"); }
            return false;
        }

        try_hr!((vt::<AudioClientVtbl>(client).Initialize)(client, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, BUFFER_DUR_100NS, 0, fmt_ptr, std::ptr::null()), "Initialize", false);

        let event = CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null());
        if event.is_null() { if PRINT_AUDIO { eprintln!("audio: CreateEventW failed"); } return false; }
        *event_out = event;
        try_hr!((vt::<AudioClientVtbl>(client).SetEventHandle)(client, event), "SetEventHandle", false);

        let mut buf_frames_n: u32 = 0;
        try_hr!((vt::<AudioClientVtbl>(client).GetBufferSize)(client, &mut buf_frames_n), "GetBufferSize", false);

        let mut render: *mut c_void = std::ptr::null_mut();
        try_hr!((vt::<AudioClientVtbl>(client).GetService)(client, &IID_IAudioRenderClient, &mut render), "GetService(IAudioRenderClient)", false);
        *render_out = render;

        if super::PRINT_AUDIO_TIMING { eprintln!("[{}ms] calling Start", super::tms()); }
        try_hr!((vt::<AudioClientVtbl>(client).Start)(client), "Start", false);
        if super::PRINT_AUDIO_TIMING { eprintln!("[{}ms] Start returned", super::tms()); }

        // Only now is the stream actually rolling; readiness before this point would make
        // sounds queue behind Start's variable internal setup and come out bunched.
        AUDIO.lock().unwrap().device_rate = rate;
        if PRINT_AUDIO { eprintln!("audio: wasapi up, {rate}hz {ch_n}ch, {buf_frames_n} frame buffer"); }

        let mut started = false;
        loop {
            WaitForSingleObject(event, 200); // The engine signals every 10ms quantum; timeout is a safety net

            if DEFAULT_CHANGED.swap(false, Ordering::Relaxed) {
                if PRINT_AUDIO { eprintln!("audio: default device changed, reopening"); }
                return true;
            }

            let mut pad: u32 = 0;
            let hr = (vt::<AudioClientVtbl>(client).GetCurrentPadding)(client, &mut pad);
            if hr < 0 {
                if PRINT_AUDIO { eprintln!("audio: device lost (hr={:#010x}), reopening", hr as u32); }
                return true;
            }
            if super::PRINT_AUDIO_TIMING && started && pad == 0 { eprintln!("[{}ms] underrun (buffer drained)", super::tms()); }

            let free_n = buf_frames_n - pad;
            if free_n > 0 {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                if (vt::<AudioRenderClientVtbl>(render).GetBuffer)(render, free_n, &mut ptr) < 0 { return true; }
                let buf = std::slice::from_raw_parts_mut(ptr as *mut f32, free_n as usize * ch_n);
                mix_into(buf, ch_n);
                if (vt::<AudioRenderClientVtbl>(render).ReleaseBuffer)(render, free_n, 0) < 0 { return true; }
                started = true;
            }
        }
    }
}
