//! Session description (RFC 4566) for the one stream we serve: H.264 as
//! RFC 6184 packetization-mode 1, plus G.711 a-law when the source has it.

use crate::media::{nal_type, StreamInfo};
use std::fmt::Write as _;
use std::net::IpAddr;

/// Per-track `a=control` names, indexed by track id (0 video, 1 audio).
pub const CONTROLS: [&str; 2] = ["trackID=0", "trackID=1"];

pub const VIDEO_PT: u8 = 96;
pub const AUDIO_PT: u8 = 8;

/// Audio is offered only as PCMA (static PT 8); this camera never makes
/// anything else, and a mislabelled codec is worse than no audio.
pub fn has_audio(info: &StreamInfo) -> bool {
    matches!(&info.audio, Some(a) if a.alaw)
}

/// The control names valid for this stream.
pub fn controls(info: &StreamInfo) -> &'static [&'static str] {
    if has_audio(info) {
        &CONTROLS
    } else {
        &CONTROLS[..1]
    }
}

/// `profile-level-id` straight from SPS bytes 1..4 (profile_idc,
/// constraint flags, level_idc). Falls back to Constrained Baseline 3.1 when
/// the SPS is missing or does not look like one.
pub fn profile_level_id(sps: &[u8]) -> String {
    let known = |p: u8| {
        matches!(p, 66 | 77 | 88 | 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
    };
    if sps.len() >= 4 && nal_type(sps) == 7 && known(sps[1]) && sps[3] != 0 {
        format!("{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3])
    } else {
        "42e01f".to_string()
    }
}

pub fn build(info: &StreamInfo, origin: u64, local: IpAddr) -> String {
    let (fam, any) = match local {
        IpAddr::V4(_) => ("IP4", "0.0.0.0"),
        IpAddr::V6(_) => ("IP6", "::"),
    };
    // s= is free text, but never let a stream name break the line structure.
    let name = if !info.name.is_empty() && info.name.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        info.name.as_str()
    } else {
        "camrtsp"
    };
    let mut s = String::with_capacity(512);
    let _ = write!(
        s,
        "v=0\r\no=- {} 1 IN {} {}\r\ns={}\r\nc=IN {} {}\r\nt=0 0\r\n\
         a=tool:camrtsp\r\na=range:npt=0-\r\na=control:*\r\n",
        origin, fam, local, name, fam, any
    );
    let _ = write!(
        s,
        "m=video 0 RTP/AVP {pt}\r\na=rtpmap:{pt} H264/90000\r\n\
         a=fmtp:{pt} packetization-mode=1;profile-level-id={}",
        profile_level_id(&info.sps),
        pt = VIDEO_PT
    );
    if !info.sps.is_empty() && !info.pps.is_empty() {
        let _ = write!(
            s,
            ";sprop-parameter-sets={},{}",
            crate::b64::encode(&info.sps),
            crate::b64::encode(&info.pps)
        );
    }
    s.push_str("\r\n");
    if info.fps > 0 {
        let _ = write!(s, "a=framerate:{}\r\n", info.fps);
    }
    if info.width > 0 && info.height > 0 {
        let _ = write!(s, "a=x-dimensions:{},{}\r\n", info.width, info.height);
    }
    let _ = write!(s, "a=control:{}\r\n", CONTROLS[0]);
    if let (true, Some(a)) = (has_audio(info), &info.audio) {
        let _ = write!(
            s,
            "m=audio 0 RTP/AVP {pt}\r\na=rtpmap:{pt} PCMA/{}/{}\r\na=control:{}\r\n",
            if a.rate > 0 { a.rate } else { 8000 },
            a.channels.max(1),
            CONTROLS[1],
            pt = AUDIO_PT
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::AudioInfo;

    #[test]
    fn profile_from_sps_or_fallback() {
        // The real /vs0 SPS (Main profile, level 4.0).
        let vs0 = [0x27, 0x4d, 0xe0, 0x28, 0x8d, 0x68, 0x05, 0x00];
        assert_eq!(profile_level_id(&vs0), "4de028");
        assert_eq!(profile_level_id(&[0x67, 0x42, 0xc0, 0x1e, 0xda]), "42c01e");
        assert_eq!(profile_level_id(&[]), "42e01f");
        assert_eq!(profile_level_id(&[0x68, 0x42, 0xc0, 0x1e]), "42e01f");
        assert_eq!(profile_level_id(&[0x67, 0x13, 0xc0, 0x1e]), "42e01f");
    }

    #[test]
    fn sdp_lines() {
        let mut info = StreamInfo {
            name: "vs0".into(),
            sps: vec![0x67, 0x42, 0xc0, 0x1e],
            pps: vec![0x68, 0xce, 0x38, 0x80],
            width: 1280,
            height: 720,
            fps: 15,
            audio: None,
        };
        let s = build(&info, 7, "127.0.0.1".parse().unwrap());
        assert!(s.starts_with("v=0\r\no=- 7 1 IN IP4 127.0.0.1\r\n"));
        assert!(s.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(s.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(s.contains(
            "a=fmtp:96 packetization-mode=1;profile-level-id=42c01e;sprop-parameter-sets=Z0LAHg==,aM44gA==\r\n"
        ));
        assert!(!s.contains("m=audio"));
        info.audio = Some(AudioInfo { rate: 8000, channels: 1, alaw: true });
        let s = build(&info, 7, "::1".parse().unwrap());
        assert!(s.contains("IN IP6 ::1"));
        assert!(s.contains("m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=control:trackID=1\r\n"));
        assert_eq!(controls(&info).len(), 2);
    }
}
