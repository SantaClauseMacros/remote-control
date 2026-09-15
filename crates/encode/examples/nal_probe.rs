//! Dump the streaming encoder's output packet by packet — each record is a
//! `u32` little-endian length, a `u8` keyframe flag, then the packet bytes —
//! so the bitstream can be inspected with packet boundaries intact (slices
//! per packet, frame types, SPS/VUI fields). `stream_probe` writes one
//! concatenated `.h264`, which loses exactly that.
//!
//! ```text
//! cargo run -p rc-encode --example nal_probe --release -- packets.bin 180 60
//! ```

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use rc_capture::{Capturer, D3dContext, Grab};
use rc_encode::{EncodeConfig, EncodedPacket, MediaFoundation, StreamEncoder};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn write_packet(file: &mut BufWriter<std::fs::File>, p: &EncodedPacket) -> Result<()> {
    file.write_all(&(p.data.len() as u32).to_le_bytes())?;
    file.write_all(&[p.key_frame as u8])?;
    file.write_all(&p.data)?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let mut a = std::env::args().skip(1);
    let out = a.next().unwrap_or_else(|| "packets.bin".into());
    let frames: u64 = a.next().and_then(|s| s.parse().ok()).unwrap_or(180);
    let fps: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(60);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _mf = MediaFoundation::init()?;

    let mut cap = Capturer::new(D3dContext::new()?, 0)?;
    let (w, h) = (cap.width() as usize, cap.height() as usize);
    let cfg = EncodeConfig::balanced(w as u32, h as u32, fps);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let enc = StreamEncoder::new(cfg, tx)?;
    println!("{w}x{h}@{fps} hardware={}", enc.is_hardware());

    let mut file = BufWriter::new(std::fs::File::create(&out)?);
    let frame_dur = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut last: Option<Vec<u8>> = None;
    let mut ts_us = 0u64;
    let mut got = 0u64;

    for i in 0..frames {
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
            // A moving marker so every frame carries real change even when the
            // desktop is static — otherwise the encoder emits nothing but skips.
            let mut b = bgra.clone();
            let row = (i as usize * 13) % (h - 16);
            for y in row..row + 16 {
                for x in 0..w.min(400) {
                    let o = (y * w + x) * 4;
                    b[o] = (i as u8).wrapping_mul(7);
                    b[o + 1] = 255 - (x as u8);
                    b[o + 2] = (y as u8).wrapping_mul(3);
                }
            }
            enc.submit_bgra(b, ts_us)?;
            ts_us += 1_000_000 / fps as u64;
        }
        while let Ok(p) = rx.try_recv() {
            got += 1;
            write_packet(&mut file, &p)?;
        }
    }

    drop(enc); // flushes the encoder
    while let Some(p) = rx.blocking_recv() {
        got += 1;
        write_packet(&mut file, &p)?;
    }
    file.flush()?;
    println!("wrote {got} packets to {out}");
    Ok(())
}
