//! Full video-pipeline check in one process:
//! capture → streaming H.264 encode → decode → BGRA, dumped to
//! `roundtrip.bmp`. If that image looks like your screen, the whole video path
//! is sound.
//!
//! ```text
//! cargo run -p rc-decode --example roundtrip --release
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rc_capture::{Capturer, D3dContext, Grab};
use rc_decode::StreamDecoder;
use rc_encode::{EncodeConfig, MediaFoundation, StreamEncoder};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _mf = MediaFoundation::init()?;

    let d3d = D3dContext::new()?;
    let mut cap = Capturer::new(d3d, 0)?;
    let (w, h) = (cap.width(), cap.height());
    let cfg = EncodeConfig::balanced(w, h, 30);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let enc = StreamEncoder::new(cfg, tx)?;
    let mut dec = StreamDecoder::new()?;
    println!(
        "encoder = {}",
        if enc.is_hardware() {
            "HARDWARE"
        } else {
            "software"
        }
    );

    // Feed ~60 frames so we get past the first keyframe and a few deltas.
    let mut ts = 0u64;
    let mut last = vec![0u8; (w * h * 4) as usize];
    let deadline = Instant::now() + Duration::from_secs(4);
    for _ in 0..60 {
        while let Grab::Frame(f) = cap.grab(4)? {
            if !f.mouse_only {
                last = f.bgra;
            }
        }
        enc.submit_bgra(last.clone(), ts)?;
        ts += 33_333;
        std::thread::sleep(Duration::from_millis(30));
        if Instant::now() > deadline {
            break;
        }
    }
    drop(enc);

    let mut decoded = 0;
    let mut best: Option<(u32, u32, Vec<u8>)> = None;
    while let Some(pkt) = rx.blocking_recv() {
        if let Some(frame) = dec.decode(&pkt.data)? {
            decoded += 1;
            best = Some((frame.width, frame.height, frame.bgra));
        }
    }

    let (bw, bh, bgra) = best.ok_or_else(|| anyhow!("decoder produced no frames"))?;
    println!("decoded {decoded} frames; last is {bw}x{bh}");
    write_bmp("roundtrip.bmp", bw, bh, &bgra)?;
    println!("wrote roundtrip.bmp");
    Ok(())
}

fn write_bmp(path: &str, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
    let px = (w * h * 4) as usize;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"BM")?;
    f.write_all(&(54 + px as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(-(h as i32)).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(px as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&bgra[..px])?;
    f.flush()?;
    Ok(())
}
