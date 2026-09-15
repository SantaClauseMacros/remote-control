//! Play a few seconds of a test tone through `render_mic`, so the mic
//! playback path (device pick, format, buffering) can be checked without a
//! phone attached. You should hear a tone on this PC's speakers, or see a
//! "CABLE" device picked in the log if a virtual audio cable is installed.
//!
//! ```text
//! cargo run -p rc-audio --example mic_render_probe --release -- 4
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let rate = 48_000u32;

    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel();

    // 20ms mono chunks of a 440Hz tone, matching the ~20ms packets the real
    // mic pipeline sends.
    let block = (rate / 50) as usize;
    let mut phase = 0.0f64;
    let step = 2.0 * std::f64::consts::PI * 440.0 / f64::from(rate);
    for _ in 0..(seconds * 50) {
        let samples: Vec<i16> = (0..block)
            .map(|_| {
                let v = (phase.sin() * 8000.0) as i16;
                phase += step;
                v
            })
            .collect();
        tx.send((samples, rate, 1usize))?;
    }
    drop(tx);

    // Blocks here, playing every queued chunk in order, and returns once the
    // channel is empty and the sender (dropped above) is gone.
    rc_audio::render_mic(&stop, &rx);
    println!("done — did you hear a tone?");
    Ok(())
}
