//! Media plumbing: the frame types that cross module boundaries, the
//! SPS/PPS finder, and an Annex-B splitter that accepts both start-code
//! (*.h264 dumps, what venc produces) and 4-byte length-prefixed NALs
//! (what some SDK paths hand out).

/// Our version, kept out of `env!("CARGO_PKG_VERSION")` so a bare
/// `rustc src/main.rs` (no cargo) still compiles.
pub const VERSION: &str = "0.1.0";

/// One access unit: the NAL units that belong to a single coded picture.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    /// NAL units, start codes stripped. On a keyframe this includes SPS/PPS
    /// as the encoder emits them (we do not strip them out).
    pub nals: Vec<Vec<u8>>,
    pub keyframe: bool,
    /// Capture timestamp in milliseconds (monotonic, our own clock).
    pub ts_ms: u64,
    /// Monotonic sequence number, 0-based, +1 per access unit.
    pub seq: u64,
}

impl VideoFrame {
    pub fn len(&self) -> usize {
        self.nals.iter().map(|n| n.len()).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.nals.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct AudioInfo {
    pub rate: u32,
    pub channels: u32,
    /// G.711 a-law (PT 8). Only a-law is ever sent on this camera.
    pub alaw: bool,
}

#[derive(Clone, Debug)]
pub struct AudioFrame {
    pub data: Vec<u8>,
    pub ts_ms: u64,
    pub seq: u64,
}

/// Everything the RTSP/SDP layer needs to know about the stream.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub name: String, // mount point without slash, e.g. "vs0"
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio: Option<AudioInfo>,
}

/// NAL unit type, low 5 bits of the first byte.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

pub fn is_vcl(t: u8) -> bool {
    (1..=5).contains(&t)
}

/// Walk an Annex-B byte stream and return the NAL units, start codes stripped.
/// Accepts 3- and 4-byte start codes. If the data has no start code at all,
/// returns it as a single NAL.
pub fn split_annexb(data: &[u8]) -> Vec<Vec<u8>> {
    const SC: [u8; 4] = [0, 0, 0, 1];
    const SC3: [u8; 3] = [0, 0, 1];
    let mut out = Vec::new();
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (offset of payload, len of start code)
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i..].starts_with(&SC) {
            starts.push((i + 4, 4));
            i += 4;
        } else if data[i..].starts_with(&SC3) {
            starts.push((i + 3, 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        if !data.is_empty() {
            let mut end = data.len();
            while end > 1 && data[end - 1] == 0 {
                end -= 1;
            }
            out.push(data[..end].to_vec());
        }
        return out;
    }
    for (idx, &(payload, _)) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).map(|&(next, sc)| next - sc).unwrap_or(data.len());
        if end > payload {
            out.push(data[payload..end].to_vec());
        }
    }
    out
}

/// Fallback for length-prefixed (AVCC/avcC) payloads: 4-byte big-endian
/// lengths. Returns None if the lengths don't add up exactly.
pub fn split_length_prefixed(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let n = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if n == 0 || i + n > data.len() {
            return None;
        }
        out.push(data[i..i + n].to_vec());
        i += n;
    }
    if i == data.len() && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// Split one encoder access unit into NAL units however it is framed.
pub fn split_au(data: &[u8]) -> Vec<Vec<u8>> {
    let nals = split_annexb(data);
    if nals.len() > 1 {
        return nals;
    }
    if let Some(v) = split_length_prefixed(data) {
        return v;
    }
    nals
}

/// Find SPS (7) and PPS (8) in a list of NALs — keyframes carry them.
pub fn find_sps_pps(nals: &[Vec<u8>]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for n in nals {
        match nal_type(n) {
            7 => sps = Some(n.clone()),
            8 => pps = Some(n.clone()),
            _ => {}
        }
    }
    (sps, pps)
}

/// Parse width/height out of an SPS by brute-force emulation-free trick:
/// we do not implement a full bitstream parser (the camera tells us the
/// resolution anyway). Returns None if it cannot be read.
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    // Bits after the 1-byte NAL header, skipping profile_idc(8),
    // constraint flags(8), level_idc(8), seq_parameter_set_id(ue).
    struct Bits<'a> {
        d: &'a [u8],
        pos: usize,
    }
    impl<'a> Bits<'a> {
        fn bit(&mut self) -> Option<u32> {
            let byte = *self.d.get(self.pos / 8)?;
            let b = (byte >> (7 - (self.pos % 8))) & 1;
            self.pos += 1;
            Some(b as u32)
        }
        fn ue(&mut self) -> Option<u32> {
            let mut zeros = 0;
            while self.bit()? == 0 {
                zeros += 1;
                if zeros > 31 {
                    return None;
                }
            }
            let mut v = 0u32;
            for _ in 0..zeros {
                v = (v << 1) | self.bit()?;
            }
            Some((1u32 << zeros) - 1 + v)
        }
        fn se(&mut self) -> Option<i32> {
            let v = self.ue()?;
            let k = (v as i32 + 1) / 2;
            Some(if v % 2 == 1 { k } else { -k })
        }
    }
    if sps.len() < 4 || nal_type(sps) != 7 {
        return None;
    }
    let mut b = Bits { d: &sps[1..], pos: 0 };
    let profile = b.bit()? * 0 + {
        // read profile_idc (8 bits), constraint (8), level (8)
        let mut p = 0u32;
        for _ in 0..8 {
            p = (p << 1) | b.bit()?;
        }
        p
    };
    // profile
    let _ = profile;
    // skip constraint_set flags (8) and level_idc (8)
    for _ in 0..16 {
        b.bit()?;
    }
    let _sps_id = b.ue()?;
    let profile = sps[1];
    if !matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        // baseline / main / high-without-chroma-info
    } else {
        let _chroma_format_idc = b.ue()?;
        if _chroma_format_idc == 3 {
            b.bit()?;
        }
        let _bit_depth_luma = b.ue()?;
        let _bit_depth_chroma = b.ue()?;
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            let n = b.ue()?;
            for i in 0..n.min(8) {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    let _log2_max_frame_num = b.ue()?;
    let poc_type = b.ue()?;
    if poc_type == 0 {
        let _log2_max_poc = b.ue()?;
    } else if poc_type == 1 {
        b.bit()?;
        b.se()?;
        b.se()?;
        let n = b.ue()?;
        for _ in 0..n.min(255) {
            b.se()?;
        }
    }
    let _max_num_ref_frames = b.ue()?;
    b.bit()?; // gaps_in_frame_num_allowed
    let w_mbs = b.ue()? + 1;
    let h_map = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?; // direct_8x8_inference
    let mut crop = [0u32; 4];
    if b.bit()? == 1 {
        for c in crop.iter_mut() {
            *c = b.ue()?;
        }
    }
    let cw = if frame_mbs_only == 1 { 1 } else { 2 };
    let width = w_mbs * 16 - 2 * (crop[0] + crop[1]);
    let height = h_map * 16 * cw - 2 * cw * (crop[2] + crop[3]);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_three_and_four_byte_codes() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0, 0, 0, 1, 0x67, 1, 2]);
        d.extend_from_slice(&[0, 0, 1, 0x68, 3]);
        d.extend_from_slice(&[0, 0, 0, 1, 0x65, 4, 5, 6]);
        let n = split_annexb(&d);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0], vec![0x67, 1, 2]);
        assert_eq!(n[1], vec![0x68, 3]);
        assert_eq!(n[2], vec![0x65, 4, 5, 6]);
    }

    #[test]
    fn split_no_start_code_is_one_nal() {
        let n = split_annexb(&[0x41, 9, 9]);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0], vec![0x41, 9, 9]);
    }

    #[test]
    fn length_prefixed() {
        let mut d = Vec::new();
        d.extend_from_slice(&3u32.to_be_bytes());
        d.extend_from_slice(&[0x67, 1, 2]);
        d.extend_from_slice(&2u32.to_be_bytes());
        d.extend_from_slice(&[0x68, 3]);
        let n = split_length_prefixed(&d).expect("valid");
        assert_eq!(n.len(), 2);
        assert_eq!(n[1], vec![0x68, 3]);
        assert!(split_length_prefixed(&[0, 0, 0, 9, 1]).is_none());
    }

    #[test]
    fn types() {
        assert_eq!(nal_type(&[0x67]), 7);
        assert!(is_vcl(nal_type(&[0x65])));
        assert!(!is_vcl(nal_type(&[0x67])));
    }
}
