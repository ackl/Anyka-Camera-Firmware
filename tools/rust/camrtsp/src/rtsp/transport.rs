//! The `Transport:` request header (RFC 2326 §12.39).
//!
//! We serve unicast only, to the address the control connection came from:
//! `destination=` is ignored on purpose, so a client cannot point our UDP
//! stream at a third party. Multicast and `mode=record` are refused (461).

#[derive(Debug, PartialEq)]
pub enum Lower {
    /// RTP/AVP/TCP, optionally with explicit `interleaved=` channels.
    Tcp(Option<(u8, u8)>),
    /// RTP/AVP[/UDP] with the client's RTP and RTCP ports.
    Udp { rtp: u16, rtcp: u16 },
}

/// First acceptable spec in a comma-separated list (clients may offer
/// several in preference order). `None` → 461.
pub fn parse(h: &str) -> Option<Lower> {
    h.split(',').find_map(|spec| one(spec.trim()))
}

fn one(spec: &str) -> Option<Lower> {
    let mut it = spec.split(';');
    let proto = it.next()?.trim();
    let tcp = if proto.eq_ignore_ascii_case("RTP/AVP/TCP") {
        true
    } else if proto.eq_ignore_ascii_case("RTP/AVP") || proto.eq_ignore_ascii_case("RTP/AVP/UDP") {
        false
    } else {
        return None;
    };
    let mut inter = None;
    let mut cport = None;
    for p in it {
        let p = p.trim();
        let (k, v) = match p.find('=') {
            Some(i) => (&p[..i], Some(p[i + 1..].trim())),
            None => (p, None),
        };
        if k.eq_ignore_ascii_case("multicast") {
            return None;
        } else if k.eq_ignore_ascii_case("interleaved") {
            let (a, b) = pair(v?)?;
            let a = u8::try_from(a).ok()?;
            let b = match b {
                Some(b) => u8::try_from(b).ok()?,
                None => a.checked_add(1)?,
            };
            inter = Some((a, b));
        } else if k.eq_ignore_ascii_case("client_port") {
            let (a, b) = pair(v?)?;
            let a = u16::try_from(a).ok()?;
            let b = match b {
                Some(b) => u16::try_from(b).ok()?,
                None => a.checked_add(1)?,
            };
            cport = Some((a, b));
        } else if k.eq_ignore_ascii_case("mode") {
            if !v?.trim_matches('"').eq_ignore_ascii_case("play") {
                return None;
            }
        }
    }
    if tcp {
        Some(Lower::Tcp(inter))
    } else {
        let (rtp, rtcp) = cport?;
        if rtp == 0 || rtcp == 0 {
            return None;
        }
        Some(Lower::Udp { rtp, rtcp })
    }
}

fn pair(v: &str) -> Option<(u32, Option<u32>)> {
    match v.split_once('-') {
        Some((a, b)) => Some((a.trim().parse().ok()?, Some(b.trim().parse().ok()?))),
        None => Some((v.trim().parse().ok()?, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports() {
        assert_eq!(parse("RTP/AVP/TCP;unicast;interleaved=0-1"), Some(Lower::Tcp(Some((0, 1)))));
        assert_eq!(parse("RTP/AVP/TCP;unicast;interleaved=4"), Some(Lower::Tcp(Some((4, 5)))));
        assert_eq!(parse("RTP/AVP/TCP;unicast"), Some(Lower::Tcp(None)));
        assert_eq!(
            parse("RTP/AVP;unicast;client_port=5000-5001"),
            Some(Lower::Udp { rtp: 5000, rtcp: 5001 })
        );
        assert_eq!(
            parse("RTP/AVP/UDP;unicast;client_port=5000;mode=\"PLAY\""),
            Some(Lower::Udp { rtp: 5000, rtcp: 5001 })
        );
        // preference list: first acceptable wins
        assert_eq!(
            parse("RTP/SAVP;unicast;client_port=1-2, RTP/AVP/TCP;interleaved=2-3"),
            Some(Lower::Tcp(Some((2, 3))))
        );
        assert_eq!(parse("RTP/AVP;multicast;port=5000-5001"), None);
        assert_eq!(parse("RTP/AVP;unicast"), None);
        assert_eq!(parse("RTP/AVP;unicast;client_port=0-1"), None);
        assert_eq!(parse("RTP/AVP/TCP;interleaved=300-301"), None);
        assert_eq!(parse("RTP/AVP/TCP;interleaved=255"), None);
        assert_eq!(parse("RTP/AVP/TCP;interleaved=x"), None);
        assert_eq!(parse("RTP/AVP/TCP;mode=record"), None);
        assert_eq!(parse("RAW/RAW/UDP;unicast"), None);
        assert_eq!(parse(""), None);
    }
}
