// Interactive soak: runs forever making sound so the backend can be poked at by ear.
//   - Switch/unplug/replug outputs: sound follows the new default without a gap, and a second
//     "up" line during a switch means routing failed.
//   - Stop and resume the process (Ctrl+Z, wait a few seconds, fg): the fanfare must jump
//     forward as if it had kept playing unheard. Picking up exactly where it stopped means the
//     sink never reported the underrun and gap detection is blind on that path.
//   - Leave it running: the beep cadence must stay metronomic.
// Press Enter (or Ctrl+C) to quit.

use zebra_gui as viz;
use zebra_gui::audio;

fn sleep_ms(ms: u64) { std::thread::sleep(std::time::Duration::from_millis(ms)); }

fn main() {
    viz::setup_audio();

    std::thread::spawn(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(n) if n > 0 => std::process::exit(0),
            _ => {} // Stdin is EOF/detached; Ctrl+C is the only quit
        }
    });

    println!("Beeps every half second (G4 G4 G4 C5), fanfare every 20s, forever.");
    println!("Switch devices, or Ctrl+Z / fg, and listen. Enter (or Ctrl+C) quits.");
    let mut tick_i = 0u32;
    loop {
        if audio::device_ready() {
            if tick_i % 40 == 0 { println!("fanfare"); viz::play_sound(viz::SOUND_TRUMPET_FANFARE1, 0.8, 1.0); }
            let hz = if tick_i % 4 == 3 { 523.25 } else { 392.00 };
            audio::play_sine(hz, 0.15, 0.4, 1.0);
            tick_i += 1;
        }
        sleep_ms(500);
    }
}
