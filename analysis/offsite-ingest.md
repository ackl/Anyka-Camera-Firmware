# Offsite ingest: the camera pushes its own footage out

Status 2026-11-08: **working end to end on the camera over the LAN**, and over real
TLS to the public internet. One dashboard entry short of working offsite.
Everything here is SD-card and host side; nothing has written flash.

## What the user asked for

> motion detection based recording — which will then be batch offloaded to our
> homelab storage … it doesnt need to constantly record, only when motion is
> detected … the camera pushes outbound

Offsite, the camera sits behind a Huawei E5885 (LTE Cat 6, CGNAT, no VPN client,
no DMZ). Nothing can reach in, so the camera must dial out to us.

## Why the camera speaks TLS, which it had never done

Every public path into the house is a Cloudflare Tunnel, and there is no public
IP to point a DNS record at. A camera-originated plain-HTTP push therefore has no
route: the only door is Cloudflare's edge, and that edge requires TLS.

The board does have TLS — `/lib/libssl.so.1.0.0` and `/usr/lib/libcrypto.so.1.0.0`
plus its own `libcrypto`, shipped for the P2P stack — but it is uClibc and our
binary is `armv5te-unknown-linux-musleabi`, so it is unlinkable (same wall as
`analysis/encoder-ownership.md`). Pure-Rust TLS would mean hand-writing X.509
parsing and RSA/ECDSA signature verification.

So: **mbedTLS 2.28.8, cross-built with `zig cc -target arm-linux-musleabi
-mcpu=arm926ej_s`, behind a six-function shim** (`tools/tls/`). TLS 1.2 only;
TLS 1.3 is a mbedTLS 3.x feature. Binary grew 3,463,012 → 3,945,044 B.

### Three traps that cost the afternoon

1. **`mbedtls_x509_crt_parse` sniffs PEM with `buf[buflen-1] == '\0'`**
   (`library/x509_crt.c`). Rust's `include_bytes!` has no terminator, so a valid
   CA bundle is treated as DER and rejected with `-0x2180 invalid format`. The
   shim copies the bundle and terminates it; callers never see this. This is the
   only reason `camtls_open` takes a length *and* fakes a NUL.
2. **A Rust `&str` is not NUL-terminated.** Passing `host.as_ptr()` to
   `mbedtls_ssl_set_hostname` made mbedTLS `strlen` past the end: the SNI went out
   as `basestar.ackl.mebin` and Cloudflare answered `handshake_failure` (alert
   40), which looks exactly like a policy rejection. `CString` fixes it. If a
   probe fails with alert 40 before any ServerHello, dump the record with
   `CAMTLS_DEBUG=1` and read the SNI before believing anything else.
3. **A socket timeout is a fatal error, not `WANT_READ`.** Our fds carry
   `SO_RCVTIMEO`; mapping `EAGAIN` to `MBEDTLS_ERR_SSL_WANT_READ` makes mbedTLS
   spin forever. The shim returns `MBEDTLS_ERR_NET_RECV_FAILED`.

`CAMTLS_DEBUG=1` prints mbedTLS's handshake trace to stderr, and `camrtsp
tlsprobe HOST[:PORT]` answers "does TLS work from *this* box right now" — both
exist because the board has no packet capture.

## Verification (what is actually proven)

| Claim | Evidence |
|---|---|
| TLS 1.2 client works, certificates verified | `tlsprobe` on firefly: `basestar.ackl.me`, `git.basestar.tech`, `cloudflare.com`, `www.google.com`, `letsencrypt.org` all `verified via TLSv1.2` with the embedded roots; a self-signed peer is rejected (`-0x2700`) |
| …on ARMv5TE, soft-float, 64 MB | `camrtsp tlsprobe basestar.ackl.me` **on the board**: `TLSv1.2 TLS-ECDHE-ECDSA-WITH-CHACHA20-POLY1305-SHA256 (6 roots embedded)` |
| HTTP-over-TLS to the receiver | host: 7.7 MB segment uploaded, sha256 echoed, local copy deleted; received sidecar intact (902 AUs, 20 keyframes, 1280x720, 60066 ms) |
| Receiving service | 23 files / 71 MB in `/srv/storage/cam/rec`, per-file sha256 in `journalctl -u camingest` |
| Upload from the camera itself | on the board over LAN HTTP: `upload: 29 considered, 3 uploaded (8388608 B, 0 resumed), 3 deleted, 0 failed, capped` |
| The public leg: camera → Cloudflare → tunnel → Caddy → receiver | **run 2026-11-08.** Board: `tlsprobe liu.ackl.me` → `verified via TLSv1.2 …CHACHA20-POLY1305-SHA256 (6 roots embedded)`. Camera: `upload: 1 considered, 1 uploaded (1235619 B, 0 resumed), 1 deleted, 0 failed`. Receiver journal: `upload 20261108-234011.h264 video 1235264 bytes sha256 0640f73f…` |
| No public IP anywhere in the path | `liu.ackl.me` resolves to Cloudflare anycast (104.21.92.207); tunnel and camera both dial out; nothing is port-forwarded |

## Shape of the deployment

```
camera (192.168.1.103)                     house
  camrtsp --source client:127.0.0.1:554/vs0   ← pulls the stock encoder
     ├── motion detector (AU size vs 30-frame baseline)
     ├── recorder → /mnt/Factory/rec/rec/NAME.h264 + .json (60 s, 8 GiB quota)
     └── uploader → https://liu.ackl.me/ingest → ┬ Cloudflare edge (public TLS)
                                                 └ cloudflared → Caddy .90:80, plain HTTP
                                                    → 192.168.1.240:8080
                                                       camingest → /srv/storage/cam
```

- **Receiver**: `tools/rust/camingest`, systemd unit `camingest.service` on
  storage-01 (VM105, 932 GB, 570 GB free), `--listen 0.0.0.0:8080 --root
  /srv/storage/cam --prefix /ingest`, token from `/etc/camingest.env` (0600).
  Refuses to start with an empty token. Plain HTTP by design: TLS terminates in
  Caddy.
- **Caddy**: an **`http://`** block, appended to `/opt/edge/caddy/Caddyfile`
  (backups `Caddyfile.before-liu-*`, `Caddyfile.before-logredact-*`):

      http://liu.ackl.me {
          reverse_proxy 192.168.1.240:8080 { transport http { response_header_timeout 60s } }
          request_body { max_size 9MB }
      }

  It must not be `https://`. The tunnel hands Caddy the request on port 80, and a
  `https://` site block there can only 308-redirect — to the very URL the client
  already asked for, so every request goes round the loop (`code=308`,
  `location: https://liu.ackl.me/ingest/probe.h264`). Public TLS ends at
  Cloudflare, exactly as for the other `http://` hosts in that file. The access
  log is wrapped in `format filter` to delete `X-Cam-Token` and `Authorization`:
  the camera presents its token on every request and it was landing in clear in
  `/data/access-liu.log`. Caddy's `:80` is published only on `127.0.0.1:8080`, so
  a LAN-side check is `curl -H 'Host: liu.ackl.me' http://127.0.0.1:8080/ingest/x`
  *on edge-proxy-01*; `https://192.168.1.90` is the separately published `:443`.
- **Upload protocol**: 256 KiB chunks, `X-Cam-Token`, per-file sha256 echoed by
  the receiver, local file deleted **only** on a sha match, 8 MiB per pass, one
  pass every 300 s.

### The tunnel hostname (done 2026-11-08)

The tunnel's ingress lives **in the Cloudflare dashboard** — the container runs
`cloudflared tunnel run --token …`, so there is no local config file to edit and
no way to read the ingress from the host. One public hostname now points at the
proxy:

    liu.ackl.me  →  http://192.168.1.90:80

The name is `liu`, not the `cam` this note used to assume; the hostname exists
only in the dashboard, so it was an unknown until someone looked. The `:80`
target is the load-bearing detail, and the reason for the redirect loop above.

The board's `custom/ingest.url` is `https://liu.ackl.me/ingest`. Because
Cloudflare's edge presents a public certificate, the camera verifies it with the
**embedded** roots and needs no `CAMTLS_CA`; the private Caddy root that the LAN
test needed (`custom/caddy-root.crt`) has been deleted from the card.

Checks, cheapest first:

    curl -o /dev/null -w '%{http_code}\n' https://liu.ackl.me/ingest/x   # 401 from anywhere
    curl -H "X-Cam-Token: $(cat ~/.config/anyka/ingest.token)" …        # 404 = authorised
    camrtsp tlsprobe liu.ackl.me                                        # on the board
    grep 'camrtsp: upload:' /tmp/camrtsp.log                            # then a real pass

## Clock: the 1970 problem

The board has no RTC and comes up at 1970-01-01, so segments were named
`19700101-003742.h264` — useless for ordering or retention across reboots. Busybox
has `ntpd`; `ntpd -q -p 192.168.1.254` (the house router, a literal IP because the
board's DNS is unreliable and `pool.ntp.org` does not resolve) steps the clock.
`config.sh` now runs it before starting camrtsp. Note `timeout` is not in
busybox, and `ntpd` exits 0 even when it failed to set the clock — check `date`,
not the exit code.

## Known rough edges

- **`keep_days` (14) is a silent footage killer**: expired segments are skipped
  *and never deleted*, and the recorder's quota does not know they are
  unuploaded. A receiver outage longer than two weeks loses that footage.
- **HEAD must answer `Content-Length: 0`.** `upload.rs` reads a declared length
  regardless of method, so a correct-but-literal HEAD (real length, no body) makes
  the camera sit out its full 30 s `io_timeout` on every file.
- **A 409 must never echo the offset the client just sent** — the client treats
  that as a protocol failure and abandons the file.
- New sessions still wait up to one GOP (~1.5 s) for a keyframe: the upstream is
  the stock encoder, so `request_keyframe()` is a no-op.
- Motion detection is a change detector, not an intruder detector: compressed-size
  AU vs a 30-frame median, 1.75x + 1500 B. It fired on the user moving in frame
  and wrote correct segments; on `/vs1` the byte floor should drop to ~300.
- One 60 s 720p segment is ~7.7 MB ⇒ 0.97 Mbit/s ⇒ **10.5 GB/day if it recorded
  continuously**; motion gating is what makes an LTE plan plausible.

## Reproducing

    tools/tls/build.sh all          # zig → libcamtls.a (host + armv5te)
    tools/remote-build.sh camrtsp   # ARM binary, links the TLS lib via TLS_ROOT
    tools/put camrtsp-arm /mnt/Factory/custom/camrtsp --mode 0755

`tools/tls/*.tar.gz`, `build/` and `out/` are gitignored; the tarball's sha256 is
pinned in `build.sh` (`4fef7de0…`).
