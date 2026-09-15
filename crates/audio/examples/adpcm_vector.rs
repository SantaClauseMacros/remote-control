//! Print an ADPCM test vector as JSON — a packet and the samples the Rust
//! decoder gets from it — so the browser's decoder can be checked against it.
//!
//! ```text
//! cargo run -p rc-audio --example adpcm_vector > vector.json
//! ```

fn main() {
    let pcm: Vec<i16> = (0..960)
        .flat_map(|n| {
            let t = n as f64 / 48000.0;
            let l = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 20000.0;
            let r = (2.0 * std::f64::consts::PI * 1250.0 * t).sin() * 9000.0;
            [l as i16, r as i16]
        })
        .collect();
    let mut enc = rc_audio::adpcm::Encoder::new(2, 48000);
    let _ = enc.encode(&pcm); // a second packet exercises carried-over state
    let packet = enc.encode(&pcm);
    let decoded = rc_audio::adpcm::decode(&packet).expect("decodes");
    let join = |v: &[String]| v.join(",");
    let bytes: Vec<String> = packet.iter().map(|b| b.to_string()).collect();
    let samples: Vec<String> = decoded.samples.iter().map(|s| s.to_string()).collect();
    println!("{{\"packet\":[{}],\"samples\":[{}]}}", join(&bytes), join(&samples));
}
