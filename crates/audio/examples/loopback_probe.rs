//! Capture the PC's sound for a few seconds and report what came through:
//! packets, bytes, and peak level. Play something while it runs.
//!
//! ```text
//! cargo run -p rc-audio --example loopback_probe --release -- 5
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);

    let stop = Arc::new(AtomicBool::new(false));
    let stopper = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        stopper.store(true, Ordering::Relaxed);
    });

    let start = Instant::now();
    let (mut packets, mut bytes, mut peak, mut rate) = (0u64, 0u64, 0i32, 0u32);
    rc_audio::stream_loopback(&stop, |packet| {
        packets += 1;
        bytes += packet.len() as u64;
        if let Some(d) = rc_audio::adpcm::decode(&packet) {
            rate = d.sample_rate;
            peak = peak.max(d.samples.iter().map(|&s| i32::from(s).abs()).max().unwrap_or(0));
        }
    })?;

    let secs = start.elapsed().as_secs_f64();
    println!("captured {secs:.1}s: {packets} packets, {bytes} bytes ({:.0} kbit/s), rate {rate} Hz, peak {:.1}%",
        bytes as f64 * 8.0 / secs / 1000.0, f64::from(peak) / 327.67);
    if packets == 0 {
        println!("(no packets: nothing was playing — loopback only delivers sound that's actually playing)");
    }
    Ok(())
}
