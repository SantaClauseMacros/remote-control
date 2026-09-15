//! Loopback recorder: capture the primary monitor, hardware-encode it to
//! `loopback.mp4`, and report cost. Open the file in any player to verify.
//!
//! ```text
//! cargo run -p rc-encode --example record --release -- 8 60
//! ```
//! args: <seconds> [fps]

use std::time::{Duration, Instant};

use anyhow::Result;
use rc_capture::{Capturer, D3dContext, Grab};
use rc_encode::{EncodeConfig, MediaFoundation, Mp4Recorder};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let fps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _mf = MediaFoundation::init()?;

    let d3d = D3dContext::new()?;
    let mut cap = Capturer::new(d3d, 0)?;
    let (w, h) = (cap.width(), cap.height());

    let cfg = EncodeConfig::balanced(w, h, fps);
    let mut rec = Mp4Recorder::create("loopback.mp4", cfg)?;
    println!(
        "recording {w}x{h} @ {fps}fps, {} kbps target, encoder = {}",
        cfg.bitrate_bps / 1000,
        if rec.is_hardware() {
            "HARDWARE"
        } else {
            "software"
        }
    );

    let frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let dur_100ns = (10_000_000 / fps) as i64;
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);

    let mut last_bgra: Option<Vec<u8>> = None;
    let mut frame_index: i64 = 0;
    let mut next_tick = start;
    let mut encode_time = Duration::ZERO;
    let mut worst_encode = Duration::ZERO;
    let mut captures = 0u64;

    while Instant::now() < end {
        // Drain whatever capture has for us without blocking long.
        while let Grab::Frame(f) = cap.grab(0)? {
            if !f.mouse_only {
                last_bgra = Some(f.bgra);
                captures += 1;
            }
        }

        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += frame_dur;

        if let Some(bgra) = &last_bgra {
            let pts = frame_index * dur_100ns;
            let t0 = Instant::now();
            rec.push_bgra(bgra, pts, dur_100ns)?;
            let dt = t0.elapsed();
            encode_time += dt;
            worst_encode = worst_encode.max(dt);
            frame_index += 1;
        }
    }

    let wall = start.elapsed();
    let stats = rec.finish()?;
    let file_len = std::fs::metadata("loopback.mp4")
        .map(|m| m.len())
        .unwrap_or(0);

    println!("\n--- results ---");
    println!("wall clock      : {:.2}s", wall.as_secs_f64());
    println!(
        "frames encoded  : {} ({:.1}/s)",
        stats.frames,
        stats.frames as f64 / wall.as_secs_f64()
    );
    println!("captures used   : {captures}");
    println!(
        "encoder         : {}",
        if stats.hardware {
            "HARDWARE MFT"
        } else {
            "software MFT"
        }
    );
    println!(
        "WriteSample time: {:.2} ms avg, {:.2} ms worst",
        encode_time.as_secs_f64() * 1000.0 / stats.frames.max(1) as f64,
        worst_encode.as_secs_f64() * 1000.0
    );
    println!(
        "output          : loopback.mp4  {:.2} MiB",
        file_len as f64 / 1048576.0
    );
    println!(
        "effective bitrate: {:.2} Mbps",
        (file_len as f64 * 8.0) / wall.as_secs_f64() / 1_000_000.0
    );
    Ok(())
}
