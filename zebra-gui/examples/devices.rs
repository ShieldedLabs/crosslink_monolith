// Interactive device test: beeps forever so you can unplug, replug, and switch default
// outputs while listening. Backend messages (wasapi up / device lost / default device
// changed) print as they happen. Press Enter (or Ctrl+C) to quit.

use visualizer_zcash::audio;

fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

fn main() {
    audio::setup_audio();

    std::thread::spawn(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(n) if n > 0 => std::process::exit(0),
            _ => {} // Stdin is EOF/detached; Ctrl+C is the only quit
        }
    });

    println!("Beeping forever: G4 G4 G4 C5, one per half second. Switch/unplug devices and listen.");
    println!("Enter (or Ctrl+C) quits.");
    let mut beep_i = 0u32;
    loop {
        if audio::device_ready() {
            let hz = if beep_i % 4 == 3 { 523.25 } else { 392.00 };
            audio::play_sine(hz, 0.15, 0.4, 1.0);
            beep_i += 1;
        }
        sleep_ms(500);
    }
}
