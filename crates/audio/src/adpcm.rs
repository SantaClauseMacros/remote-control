//! IMA ADPCM, one self-contained packet per ~20 ms of sound.
//!
//! Packet layout (all little-endian):
//! ```text
//! u8   version (1)
//! u8   channels (1 or 2)
//! u32  sample rate
//! u16  frames (samples per channel)
//! per channel: i16 predictor, u8 step index, u8 reserved
//! 4-bit codes: frame 0 ch 0, frame 0 ch 1, frame 1 ch 0, … — low nibble first
//! ```
//! Every packet carries the codec state it starts from, so a decoder can join
//! mid-stream or carry on after a dropped packet. The browser's decoder
//! (`decodeAdpcm` in `web/app.js`) must match this exactly.

pub const VERSION: u8 = 1;
const HEADER_LEN: usize = 8;

const STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408,
    449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630,
    9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];
const INDEX: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Debug, Clone, Copy, Default)]
struct State {
    predictor: i32,
    index: i32,
}

impl State {
    fn encode(&mut self, sample: i16) -> u8 {
        let step = STEP[self.index as usize];
        let mut diff = i32::from(sample) - self.predictor;
        let mut code = 0u8;
        if diff < 0 {
            code = 8;
            diff = -diff;
        }
        let mut delta = step >> 3;
        if diff >= step {
            code |= 4;
            diff -= step;
            delta += step;
        }
        if diff >= step >> 1 {
            code |= 2;
            diff -= step >> 1;
            delta += step >> 1;
        }
        if diff >= step >> 2 {
            code |= 1;
            delta += step >> 2;
        }
        self.advance(code, delta);
        code
    }

    fn decode(&mut self, code: u8) -> i16 {
        let step = STEP[self.index as usize];
        let mut delta = step >> 3;
        if code & 4 != 0 {
            delta += step;
        }
        if code & 2 != 0 {
            delta += step >> 1;
        }
        if code & 1 != 0 {
            delta += step >> 2;
        }
        self.advance(code, delta);
        self.predictor as i16
    }

    fn advance(&mut self, code: u8, delta: i32) {
        let next = if code & 8 != 0 {
            self.predictor - delta
        } else {
            self.predictor + delta
        };
        self.predictor = next.clamp(-32768, 32767);
        self.index = (self.index + INDEX[usize::from(code & 7)]).clamp(0, 88);
    }
}

/// Streaming encoder: the codec state runs on from one packet to the next.
pub struct Encoder {
    channels: usize,
    sample_rate: u32,
    state: Vec<State>,
}

impl Encoder {
    /// `channels` must be 1 or 2.
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let channels = channels.clamp(1, 2);
        Self {
            channels,
            sample_rate,
            state: vec![State::default(); channels],
        }
    }

    /// Encode interleaved 16-bit samples into one packet. At most `u16::MAX`
    /// frames per call; anything past that (or a trailing partial frame) is
    /// ignored.
    pub fn encode(&mut self, interleaved: &[i16]) -> Vec<u8> {
        let frames = (interleaved.len() / self.channels).min(usize::from(u16::MAX));
        let total = frames * self.channels;
        let mut out = Vec::with_capacity(HEADER_LEN + 4 * self.channels + total.div_ceil(2));
        out.push(VERSION);
        out.push(self.channels as u8);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&(frames as u16).to_le_bytes());
        for s in &self.state {
            out.extend_from_slice(&(s.predictor as i16).to_le_bytes());
            out.push(s.index as u8);
            out.push(0);
        }
        let mut low: Option<u8> = None;
        for (i, &sample) in interleaved[..total].iter().enumerate() {
            let code = self.state[i % self.channels].encode(sample);
            match low.take() {
                Some(l) => out.push(l | (code << 4)),
                None => low = Some(code),
            }
        }
        if let Some(l) = low {
            out.push(l);
        }
        out
    }
}

/// One decoded packet.
#[derive(Debug)]
pub struct Decoded {
    pub channels: usize,
    pub sample_rate: u32,
    /// Interleaved.
    pub samples: Vec<i16>,
}

/// Decode one packet; `None` if it's malformed or truncated.
pub fn decode(packet: &[u8]) -> Option<Decoded> {
    if packet.len() < HEADER_LEN || packet[0] != VERSION {
        return None;
    }
    let channels = usize::from(packet[1]);
    if !(1..=2).contains(&channels) {
        return None;
    }
    let sample_rate = u32::from_le_bytes(packet[2..6].try_into().ok()?);
    let frames = usize::from(u16::from_le_bytes(packet[6..8].try_into().ok()?));
    let mut pos = HEADER_LEN;
    let mut state = Vec::with_capacity(channels);
    for _ in 0..channels {
        let h = packet.get(pos..pos + 4)?;
        state.push(State {
            predictor: i32::from(i16::from_le_bytes([h[0], h[1]])),
            index: i32::from(h[2]).min(88),
        });
        pos += 4;
    }
    let total = frames * channels;
    let codes = packet.get(pos..pos + total.div_ceil(2))?;
    let samples = (0..total)
        .map(|i| {
            let byte = codes[i / 2];
            let code = if i % 2 == 0 { byte & 0x0f } else { byte >> 4 };
            state[i % channels].decode(code)
        })
        .collect();
    Some(Decoded {
        channels,
        sample_rate,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, channels: usize, freq: f64, rate: f64) -> Vec<i16> {
        (0..frames)
            .flat_map(|n| {
                let v = (2.0 * std::f64::consts::PI * freq * n as f64 / rate).sin() * 12000.0;
                std::iter::repeat(v as i16).take(channels)
            })
            .collect()
    }

    fn snr_db(original: &[i16], decoded: &[i16]) -> f64 {
        let signal: f64 = original.iter().map(|&s| f64::from(s).powi(2)).sum();
        let noise: f64 = original
            .iter()
            .zip(decoded)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum();
        10.0 * (signal / noise.max(1.0)).log10()
    }

    #[test]
    fn stereo_round_trip_is_clean() {
        let pcm = sine(9600, 2, 440.0, 48000.0);
        let mut enc = Encoder::new(2, 48000);
        let packet = enc.encode(&pcm);
        assert_eq!(packet.len(), HEADER_LEN + 8 + 9600);
        let out = decode(&packet).expect("decodes");
        assert_eq!((out.channels, out.sample_rate, out.samples.len()), (2, 48000, pcm.len()));
        assert!(snr_db(&pcm, &out.samples) > 25.0, "SNR {:.1} dB", snr_db(&pcm, &out.samples));
    }

    #[test]
    fn packets_decode_independently_and_continue_the_stream() {
        let pcm = sine(1920, 1, 1000.0, 48000.0);
        let mut enc = Encoder::new(1, 48000);
        let a = enc.encode(&pcm[..960]);
        let b = enc.encode(&pcm[960..]);
        // The second packet alone decodes as well as the whole stream would.
        let second = decode(&b).unwrap().samples;
        assert!(snr_db(&pcm[960..], &second) > 25.0);
        let mut joined = decode(&a).unwrap().samples;
        joined.extend(second);
        assert!(snr_db(&pcm, &joined) > 25.0);
    }

    #[test]
    fn odd_code_count_and_bad_packets() {
        let mut enc = Encoder::new(1, 22050);
        let packet = enc.encode(&[100, -200, 300]);
        assert_eq!(decode(&packet).unwrap().samples.len(), 3);
        assert!(decode(&packet[..packet.len() - 1]).is_none(), "truncated");
        assert!(decode(&[9, 1, 0, 0, 0, 0, 0, 0]).is_none(), "wrong version");
        assert!(decode(&[]).is_none());
    }
}
