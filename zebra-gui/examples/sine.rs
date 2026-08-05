// Smoke test for the audio backend: cargo run --example sine
// Expect: C4, then G4 at 1.5x speed (so ~D5-ish), then two overlapping, then a quiet one.

use visualizer_zcash::audio;

fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

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

    println!("done");
}
