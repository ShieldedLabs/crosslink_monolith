// Ogg decode test through the real play_sound path: build with --features audio.
// Expect: UI woosh, same woosh again (cached), then a staking voice line.

use zebra_gui as viz;

fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

fn main() {
    viz::setup_audio();
    for _ in 0..200 {
        if viz::audio::device_ready() { break; }
        sleep_ms(10);
    }

    println!("woosh (fresh decode)");
    viz::play_sound(viz::SOUND_UI_WOOSH, 0.8, 1.0);
    sleep_ms(900);

    println!("woosh (cached)");
    viz::play_sound(viz::SOUND_UI_WOOSH, 0.8, 1.0);
    sleep_ms(900);

    println!("voice line");
    viz::play_sound(viz::SOUND_NOW_STAKING_DAY1, 0.8, 1.0);
    sleep_ms(4000);

    println!("done");
}
