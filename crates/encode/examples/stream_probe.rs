//! Capture the primary monitor and run it through the low-latency streaming
//! encoder, reporting per-frame NAL sizes and encoder latency. Writes the raw
//! Annex-B stream to `stream.h264` (playable with ffplay / VLC).
//!
//! ```text
//! cargo run -p rc-encode --example stream_probe --release -- 5 60
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use rc_capture::{Capturer, D3dContext, Grab};
use rc_encode::{EncodeConfig, MediaFoundation, StreamEncoder};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let mut a = std::env::args().skip(1);
    let seconds: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let fps: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(60);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _mf = MediaFoundation::init()?;

    let d3d = D3dContext::new()?;
    let mut cap = Capturer::new(d3d, 0)?;
    let (w, h) = (cap.width(), cap.height());
    let cfg = EncodeConfig::balanced(w, h, fps);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let enc = StreamEncoder::new(cfg, tx)?;
    println!(
        "{w}x{h}@{fps}  {} kbps  encoder = {}",
        cfg.bitrate_bps / 1000,
        if enc.is_hardware() {
            "HARDWARE"
        } else {
            "software"
        }
    );

    let mut file = std::io::BufWriter::new(std::fs::File::create("stream.h264")?);
    let frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let mut next = start;
    let mut last: Option<Vec<u8>> = None;
    let mut submitted = 0u64;
    let mut ts_us = 0u64;

    let (mut got, mut key, mut bytes, mut worst_gap) = (0u64, 0u64, 0u64, 0.0f64);
    let mut last_pkt = Instant::now();

    while Instant::now() < end {
        while let Grab::Frame(f) = cap.grab(0)? {
            if !f.mouse_only {
                last = Some(f.bgra);
            }
        }
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += frame_dur;

        if let Some(bgra) = &last {
            enc.submit_bgra(bgra.clone(), ts_us)?;
            submitted += 1;
            ts_us += (1_000_000 / fps) as u64;
        }

        while let Ok(p) = rx.try_recv() {
            got += 1;
            if p.key_frame {
                key += 1;
            }
            bytes += p.data.len() as u64;
            let gap = last_pkt.elapsed().as_secs_f64() * 1000.0;
            worst_gap = worst_gap.max(gap);
            last_pkt = Instant::now();
            file.write_all(&p.data)?;
        }
    }

    drop(enc); // flushes the encoder
    while let Some(p) = rx.blocking_recv() {
        got += 1;
        if p.key_frame {
            key += 1;
        }
        bytes += p.data.len() as u64;
        file.write_all(&p.data)?;
    }
    file.flush()?;

    let wall = start.elapsed().as_secs_f64();
    println!("\n--- results ---");
    println!("submitted     : {submitted} frames");
    println!("encoded        : {got} packets ({key} keyframes)");
    println!(
        "output        : stream.h264  {:.2} MiB",
        bytes as f64 / 1048576.0
    );
    println!(
        "effective br  : {:.2} Mbps",
        bytes as f64 * 8.0 / wall / 1e6
    );
    println!("worst pkt gap : {worst_gap:.1} ms");
    Ok(())
}
