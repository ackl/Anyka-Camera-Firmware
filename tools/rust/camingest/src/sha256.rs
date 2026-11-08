//! SHA-256, incremental, no crates.
//!
//! The camera's uploader (`camrtsp/src/upload.rs`) implements the same thing
//! because the board has no TLS stack and no crates.io. The receiver needs it
//! for the one job that matters more than any convenience here: hashing the
//! bytes **it stored**, so the camera can be told what actually landed rather
//! than what it hoped landed. Everything in this file is therefore boring on
//! purpose — a hash you cannot trust is worse than a hash you do not have.

/// Incremental SHA-256: `update` any number of times, then `finish`.
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const K: [u32; 64] = [
    0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1, 0x923f_82a4,
    0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3, 0x72be_5d74, 0x80de_b1fe,
    0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786, 0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f,
    0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da, 0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7,
    0xc6e0_0bf3, 0xd5a7_9147, 0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc,
    0x5338_0d13, 0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
    0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070, 0x19a4_c116,
    0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a, 0x5b9c_ca4f, 0x682e_6ff3,
    0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208, 0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7,
    0xc671_78f2,
];

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            h: H0,
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        // Anything left over from the last call comes first.
        let mut data = data;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        let mut tail = [0u8; 128];
        tail[0] = 0x80;
        let pad = if self.buf_len < 56 {
            56 - self.buf_len
        } else {
            120 - self.buf_len
        };
        tail[pad..pad + 8].copy_from_slice(&bits.to_be_bytes());
        let n = pad + 8;
        // Feed the padding through the same path; `total` no longer matters.
        let mut rest = &tail[..n];
        while !rest.is_empty() {
            let take = (64 - self.buf_len).min(rest.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&rest[..take]);
            self.buf_len += take;
            rest = &rest[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.h[0], self.h[1], self.h[2], self.h[3], self.h[4], self.h[5], self.h[6], self.h[7],
        );
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
        self.h[5] = self.h[5].wrapping_add(f);
        self.h[6] = self.h[6].wrapping_add(g);
        self.h[7] = self.h[7].wrapping_add(h);
    }
}

pub fn hex_nibble(b: u8) -> u8 {
    match b & 0xf {
        n @ 0..=9 => b'0' + n,
        n => b'a' + (n - 10),
    }
}

/// Lowercase hex of a digest — the form the camera compares against.
pub fn to_hex(digest: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        s.push(hex_nibble(b >> 4) as char);
        s.push(hex_nibble(b & 0xf) as char);
    }
    s
}

pub fn hex_digest(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    to_hex(&h.finish())
}

/// 64 hex characters, either case. The camera always sends lowercase (and
/// compares echoes with `eq_ignore_ascii_case`), so anything that is hex at all
/// is acceptable here; it is only ever compared to another digest.
pub fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// The digest of a stream, and the number of bytes that went into it.
pub fn digest_reader<R: std::io::Read>(r: &mut R) -> std::io::Result<(u64, String)> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, to_hex(&h.finish())))
}

/// `(bytes, sha256 hex)` of a file, or `None` if it is not there.
pub fn digest_file(path: &std::path::Path) -> std::io::Result<Option<(u64, String)>> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match digest_reader(&mut f) {
        Ok((n, sha)) => Ok(Some((n, sha))),
        Err(e) => Err(e),
    }
}
