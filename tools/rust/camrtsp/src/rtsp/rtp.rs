//! RTP (RFC 3550) packetization for H.264 (RFC 6184: single NAL unit
//! packets and FU-A) and G.711, RTCP Sender Reports, and a validity check for
//! whatever RTCP a client sends back.
//!
//! Packets are written into one caller-owned buffer, each optionally prefixed
//! with the RFC 2326 §10.12 interleave header (`$`, channel, u16 length), and
//! their start offsets recorded in `bounds`. A TCP sender writes the whole
//! buffer with one call; a UDP sender sends each slice as a datagram. After
//! warm-up nothing here allocates.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Largest RTP payload we emit. 1400 + 12 + IP/UDP headers stays under a
/// 1500-byte Ethernet MTU with room for tunnels/VPNs.
pub const MAX_PAYLOAD: usize = 1400;

/// RTCP CNAME. RFC 3550 wants it unique per host; one host, one name.
const CNAME: &[u8] = b"camrtsp";

/// RTP state of one outgoing stream.
pub struct Rtp {
    pub pt: u8,
    pub clock: u32,
    pub ssrc: u32,
    /// Next sequence number to send.
    pub seq: u16,
    /// RTP timestamp of the first frame sent.
    pub base: u32,
    first_ms: Option<u64>,
    pub packets: u32,
    pub octets: u32,
    last_ts: u32,
    last_at: Option<Instant>,
}

impl Rtp {
    pub fn new(pt: u8, clock: u32, ssrc: u32, seq: u16, base: u32) -> Rtp {
        Rtp { pt, clock, ssrc, seq, base, first_ms: None, packets: 0, octets: 0, last_ts: base, last_at: None }
    }

    /// RTP timestamp for a frame captured at `ts_ms` on the source clock:
    /// `base` for the first frame sent, then the elapsed source time in
    /// clock ticks. Remembers the mapping for Sender Reports.
    pub fn ts_for(&mut self, ts_ms: u64) -> u32 {
        let first = *self.first_ms.get_or_insert(ts_ms);
        let d = ts_ms.saturating_sub(first);
        let ticks = d.wrapping_mul(self.clock as u64) / 1000;
        let ts = self.base.wrapping_add(ticks as u32);
        self.last_ts = ts;
        self.last_at = Some(Instant::now());
        ts
    }

    /// Append one RTP packet whose payload is the concatenation of `parts`.
    pub fn packet(
        &mut self,
        out: &mut Vec<u8>,
        bounds: &mut Vec<usize>,
        chan: Option<u8>,
        marker: bool,
        ts: u32,
        parts: &[&[u8]],
    ) {
        let plen: usize = parts.iter().map(|p| p.len()).sum();
        bounds.push(out.len());
        if let Some(ch) = chan {
            let l = ((12 + plen) as u16).to_be_bytes();
            out.extend_from_slice(&[b'$', ch, l[0], l[1]]);
        }
        out.push(0x80); // V=2, no padding, no extension, no CSRC
        out.push((if marker { 0x80 } else { 0 }) | (self.pt & 0x7f));
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        for p in parts {
            out.extend_from_slice(p);
        }
        self.seq = self.seq.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(plen as u32);
    }

    /// Append an RTCP compound packet: SR + SDES(CNAME), as RFC 3550 §6.1
    /// requires. The RTP timestamp is extrapolated from the last frame sent,
    /// which on a live source is a few ms behind the wall clock at most.
    /// With `bye`, a BYE (RFC 3550 §6.6) closes the compound: the stream is
    /// over, and ffmpeg turns a BYE on every stream into end-of-file.
    pub fn sender_report(&self, out: &mut Vec<u8>, bounds: &mut Vec<usize>, chan: Option<u8>, bye: bool) {
        let now = Instant::now();
        let rtp_now = match self.last_at {
            Some(at) => {
                let ms = now.saturating_duration_since(at).as_millis() as u64;
                self.last_ts.wrapping_add((ms * self.clock as u64 / 1000) as u32)
            }
            None => self.base,
        };
        let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let ntp_sec = (d.as_secs() + 2_208_988_800) as u32;
        let ntp_frac = (((d.subsec_nanos() as u64) << 32) / 1_000_000_000) as u32;

        // SDES chunk: SSRC, CNAME item, END, padded to 32 bits.
        let chunk = (4 + 2 + CNAME.len() + 1 + 3) & !3;
        let sdes_len = 4 + chunk;
        let total = 28 + sdes_len + if bye { 8 } else { 0 };
        bounds.push(out.len());
        if let Some(ch) = chan {
            let l = (total as u16).to_be_bytes();
            out.extend_from_slice(&[b'$', ch, l[0], l[1]]);
        }
        out.extend_from_slice(&[0x80, 200, 0, 6]);
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&ntp_sec.to_be_bytes());
        out.extend_from_slice(&ntp_frac.to_be_bytes());
        out.extend_from_slice(&rtp_now.to_be_bytes());
        out.extend_from_slice(&self.packets.to_be_bytes());
        out.extend_from_slice(&self.octets.to_be_bytes());
        let words = (sdes_len / 4 - 1) as u16;
        out.extend_from_slice(&[0x81, 202]);
        out.extend_from_slice(&words.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&[1, CNAME.len() as u8]);
        out.extend_from_slice(CNAME);
        for _ in 4 + 2 + CNAME.len()..chunk {
            out.push(0); // END item + padding
        }
        if bye {
            out.extend_from_slice(&[0x81, 203, 0, 1]);
            out.extend_from_slice(&self.ssrc.to_be_bytes());
        }
    }
}

/// Packetize one access unit. `nals` is iterated twice (to find the last
/// NAL, which carries the marker), hence `Clone`. Empty NALs are skipped by
/// the caller.
pub fn h264_au<'a, I>(
    rtp: &mut Rtp,
    nals: I,
    ts: u32,
    chan: Option<u8>,
    out: &mut Vec<u8>,
    bounds: &mut Vec<usize>,
) where
    I: Iterator<Item = &'a [u8]> + Clone,
{
    let n = nals.clone().count();
    for (i, nal) in nals.enumerate() {
        let last = i + 1 == n;
        if nal.len() <= MAX_PAYLOAD {
            rtp.packet(out, bounds, chan, last, ts, &[nal]);
            continue;
        }
        // FU-A (RFC 6184 §5.8): the NAL header is replaced by an indicator
        // (F, NRI, type 28) and a FU header (S, E, original type).
        let hdr = nal[0];
        let ind = (hdr & 0xe0) | 28;
        let mut rest = &nal[1..];
        let mut first = true;
        while !rest.is_empty() {
            let take = rest.len().min(MAX_PAYLOAD - 2);
            let end = take == rest.len();
            let fu = [ind, (if first { 0x80 } else { 0 }) | (if end { 0x40 } else { 0 }) | (hdr & 0x1f)];
            rtp.packet(out, bounds, chan, last && end, ts, &[&fu, &rest[..take]]);
            rest = &rest[take..];
            first = false;
        }
    }
}

/// Is this a well-formed RTCP compound packet? Used only to decide whether
/// an incoming packet counts as a sign of life; we act on none of them.
/// Never panics, whatever the bytes.
pub fn rtcp_valid(p: &[u8]) -> bool {
    let mut i = 0usize;
    let mut n = 0;
    while i + 4 <= p.len() {
        if p[i] >> 6 != 2 || !(192..=223).contains(&p[i + 1]) {
            return false;
        }
        let words = u16::from_be_bytes([p[i + 2], p[i + 3]]) as usize;
        let len = (words + 1) * 4;
        if len > p.len() - i {
            return false;
        }
        i += len;
        n += 1;
    }
    n > 0 && i == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split<'a>(buf: &'a [u8], bounds: &[usize]) -> Vec<&'a [u8]> {
        (0..bounds.len())
            .map(|i| &buf[bounds[i]..bounds.get(i + 1).copied().unwrap_or(buf.len())])
            .collect()
    }

    #[test]
    fn single_and_fu_a() {
        let mut r = Rtp::new(96, 90000, 0xdeadbeef, 65534, 1000);
        let small = [0x67u8, 1, 2, 3];
        let mut big = vec![0x65u8];
        big.extend((0..4000u32).map(|i| (i % 251) as u8));
        let (mut out, mut b) = (Vec::new(), Vec::new());
        let ts = r.ts_for(5000);
        assert_eq!(ts, 1000);
        h264_au(&mut r, [&small[..], &big[..]].into_iter(), ts, None, &mut out, &mut b);
        let pk = split(&out, &b);
        // 4000 payload bytes / 1398 per fragment = 3 fragments
        assert_eq!(pk.len(), 4);
        assert_eq!(&pk[0][12..], &small);
        assert_eq!(pk[0][1], 96, "no marker on SPS");
        assert_eq!(u16::from_be_bytes([pk[0][2], pk[0][3]]), 65534);
        assert_eq!(u16::from_be_bytes([pk[1][2], pk[1][3]]), 65535);
        assert_eq!(u16::from_be_bytes([pk[2][2], pk[2][3]]), 0, "seq wraps");
        let mut re = vec![(pk[1][12] & 0xe0) | (pk[1][13] & 0x1f)];
        for (i, p) in pk[1..].iter().enumerate() {
            assert!(p.len() <= 12 + MAX_PAYLOAD);
            assert_eq!(p[12] & 0x1f, 28);
            assert_eq!(p[13] & 0x80 != 0, i == 0, "S bit");
            assert_eq!(p[13] & 0x40 != 0, i == 2, "E bit");
            assert_eq!(p[1] & 0x80 != 0, i == 2, "marker on the last fragment only");
            re.extend_from_slice(&p[14..]);
        }
        assert_eq!(re, big);
        assert_eq!(r.ts_for(5040), 1000 + 40 * 90);
    }

    #[test]
    fn interleaved_framing_and_sr() {
        let mut r = Rtp::new(8, 8000, 7, 0, u32::MAX - 10);
        let (mut out, mut b) = (Vec::new(), Vec::new());
        let ts = r.ts_for(0);
        r.packet(&mut out, &mut b, Some(2), true, ts, &[&[0xd5; 160]]);
        assert_eq!(&out[..4], &[b'$', 2, 0, 172]);
        assert_eq!(out[5], 0x80 | 8);
        assert_eq!(r.ts_for(20), (u32::MAX - 10).wrapping_add(160), "timestamps wrap");
        out.clear();
        b.clear();
        r.sender_report(&mut out, &mut b, Some(3), false);
        assert_eq!(&out[..2], &[b'$', 3]);
        let len = u16::from_be_bytes([out[2], out[3]]) as usize;
        assert_eq!(len, out.len() - 4);
        assert!(rtcp_valid(&out[4..]));
        assert_eq!(out[5], 200);
        assert_eq!(u32::from_be_bytes([out[24], out[25], out[26], out[27]]), 1, "packet count");
        out.clear();
        b.clear();
        r.sender_report(&mut out, &mut b, None, true);
        assert!(rtcp_valid(&out));
        assert_eq!(&out[out.len() - 8..out.len() - 4], &[0x81, 203, 0, 1]);
    }

    #[test]
    fn rtcp_check_survives_noise() {
        // a receiver report with one block
        let mut rr = vec![0x81, 201, 0, 7];
        rr.extend_from_slice(&[0; 28]);
        assert!(rtcp_valid(&rr));
        assert!(!rtcp_valid(&rr[..20]));
        assert!(!rtcp_valid(&[]));
        assert!(!rtcp_valid(&[0x80, 96, 0, 0]));
        let mut x: u32 = 99;
        for n in 0..2000usize {
            let v: Vec<u8> = (0..n % 64)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    if x % 3 == 0 { 0x81 } else { x as u8 }
                })
                .collect();
            let _ = rtcp_valid(&v);
        }
        // length field pointing far past the end
        assert!(!rtcp_valid(&[0x80, 200, 0xff, 0xff, 0, 0, 0, 0]));
    }
}
