//! Capture probe: grab frames from the primary monitor for a few seconds,
//! report throughput, and dump one frame to `capture-probe.bmp`.
//!
//! ```text
//! cargo run -p rc-capture --example grab --release
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use rc_capture::{Capturer, D3dContext, Grab};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let seconds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);

    let d3d = D3dContext::new()?;
    let mut cap = Capturer::new(d3d, 0)?;
    println!("capturing {}x{} for {seconds}s…", cap.width(), cap.height());

    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let (mut frames, mut timeouts, mut mouse_only, mut behind) = (0u64, 0u64, 0u64, 0u64);
    let mut worst_gap = Duration::ZERO;
    let mut last_frame_at = Instant::now();
    let mut saved: Option<(u32, u32, Vec<u8>)> = None;

    while Instant::now() < deadline {
        match cap.grab(16)? {
            Grab::Frame(f) => {
                frames += 1;
                if f.mouse_only {
                    mouse_only += 1;
                }
                if f.accumulated_frames > 1 {
                    behind += 1;
                }
                let now = Instant::now();
                worst_gap = worst_gap.max(now - last_frame_at);
                last_frame_at = now;
                if saved.is_none() && !f.mouse_only {
                    saved = Some((f.width, f.height, f.bgra));
                }
            }
            Grab::Timeout => timeouts += 1,
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("\n--- results ---");
    println!("elapsed        : {elapsed:.2}s");
    println!(
        "frames         : {frames}  ({:.1}/s)",
        frames as f64 / elapsed
    );
    println!("  mouse-only   : {mouse_only}  (encoder would skip these)");
    println!("  behind (>1)  : {behind}");
    println!("timeouts       : {timeouts}  (screen was static — near-zero cost)");
    println!(
        "worst frame gap: {:.1} ms",
        worst_gap.as_secs_f64() * 1000.0
    );

    if let Some((w, h, bgra)) = saved {
        write_bmp("capture-probe.bmp", w, h, &bgra)?;
        println!("\nwrote capture-probe.bmp ({w}x{h})");
    } else {
        println!("\nno non-cursor frame captured (screen never changed?)");
    }
    Ok(())
}

/// Minimal top-down 32-bit BGRA BMP writer (no dependencies).
fn write_bmp(path: &str, w: u32, h: u32, bgra: &[u8]) -> Result<()> {
    let pixel_bytes = (w * h * 4) as usize;
    let file_size = 54 + pixel_bytes as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

    // BITMAPFILEHEADER
    f.write_all(b"BM")?;
    f.write_all(&file_size.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    // BITMAPINFOHEADER
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(-(h as i32)).to_le_bytes())?; // negative => top-down
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&32u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?; // BI_RGB
    f.write_all(&(pixel_bytes as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    f.write_all(&bgra[..pixel_bytes])?;
    f.flush()?;
    Ok(())
}
