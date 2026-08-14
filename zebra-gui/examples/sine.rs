// Smoke test for the audio backend: cargo run --example sine
// Expect: C4, then G4 at 1.5x speed (so ~D5-ish), then two overlapping, then a quiet one,
// then a left-only note and a right-only note. A swapped or strided channel order is
// instantly audible on headphones.

use zebra_gui::audio;

fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

fn note(hz: f32, secs: f32, l: f32, r: f32) -> audio::Sound {
    let rate = 48000u32;
    let frames_n = (secs * rate as f32) as usize;
    let fade_n   = usize::min(200, frames_n / 2);
    let mut frames = Vec::with_capacity(frames_n);
    for i in 0..frames_n {
        let mut s = (i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin();
        if i < fade_n             { s *= i as f32 / fade_n as f32; }
        if i >= frames_n - fade_n { s *= (frames_n - i) as f32 / fade_n as f32; }
        frames.push([s * l, s * r]);
    }
    audio::Sound { rate, frames }
}

fn main() {
    audio::setup_audio();
    for _ in 0..200 {
        if audio::device_ready() { break; }
        sleep_ms(10);
    }
    if !audio::device_ready() { println!("no audio device after 2s, bailing"); return; }

    println!("C4");
    audio::play_sine(261.63, 0.4, 0.5, 1.0);
    sleep_ms(600);

    println!("G4 at 1.5x speed (resampler path)");
    audio::play_sine(392.00, 0.4, 0.5, 1.5);
    sleep_ms(600);

    println!("C4 + E4 overlapping (mixing path)");
    audio::play_sine(261.63, 0.6, 0.4, 1.0);
    sleep_ms(150);
    audio::play_sine(329.63, 0.6, 0.4, 1.0);
    sleep_ms(800);

    println!("A4 at half master volume");
    audio::set_master_volume(0.5);
    audio::play_sine(440.00, 0.4, 0.5, 1.0);
    sleep_ms(600);

    audio::set_master_volume(1.0);
    println!("C4 left only");
    audio::play_sound_pcm(note(261.63, 0.4, 0.5, 0.0), 1.0, 1.0);
    sleep_ms(600);

    println!("C4 right only");
    audio::play_sound_pcm(note(261.63, 0.4, 0.0, 0.5), 1.0, 1.0);
    sleep_ms(600);

    println!("done");
}
