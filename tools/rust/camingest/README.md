# camingest — the receiving end of the camera's batch uploader

The camera records motion segments to its SD card and, when it can reach the
house, pushes them out chunk by chunk. This is the box that takes them.

The wire protocol is **not defined here**. It is defined by the camera, in
`tools/rust/camrtsp/src/upload.rs` (`# Wire protocol (v1) — "cam-ingest"`), and
this server implements exactly that, with the choices that file leaves open
settled on the strict side. If you change one end, change the other and read
`upload.rs` first: it is the authority.

Pure `std`, no crates, one `rustc` invocation. It runs on x86_64 (storage-01),
not on the camera.

```sh
./build.sh                       # binary + tests
./camingest serve --root /srv/storage/cam --prefix /ingest --token "$TOKEN"
```

```
camingest 0.1.0 listening on 0.0.0.0:8080 root=/srv/storage/cam/rec prefix=/ingest token=set(29 bytes) max_conns=32 max_chunk=8388608 max_json=262144
```

## CLI

| flag | default | meaning |
|---|---|---|
| `--listen ADDR` | `0.0.0.0:8080` | address to bind |
| `--root DIR` | `/srv/storage/cam` | storage root; segments land in `DIR/rec` |
| `--prefix PATH` | `/ingest` | path prefix the camera's `base_url` ends with |
| `--token T` | `$CAMINGEST_TOKEN` | **required**; the server refuses to start without it |
| `--max-conns N` | 32 | concurrent connections; beyond it the answer is 503 |
| `--max-chunk BYTES` | `8M` | largest chunk body accepted |
| `--max-json BYTES` | `256K` | largest sidecar accepted |
| `--io-timeout SECS` | 30 | socket read/write timeout |
| `--quiet` | off | no per-request lines (startup and fatal errors still print) |

`--help`, `--version`, and `camingest serve --help` all work.

One line per accepted upload, one per failure, none per chunk:

```
2026-11-08T23:10:29Z upload 20261108-230729.h264 video 1048576 bytes sha256 b8ef1c04…
2026-11-08T23:10:29Z upload 20261108-230729.json sidecar 81 bytes sha256 ac3e3e83…
2026-11-08T23:10:29Z reset 20261108-230929.h264.part dropped 1052672 bytes
2026-11-08T23:10:29Z fail auth: wrong token from 127.0.0.1:56570
```

## On disk

```
<root>/rec/<NAME>.h264          a segment. Only a verified commit creates one.
<root>/rec/<NAME>.json          its sidecar, written whole and renamed.
<root>/rec/<NAME>.h264.part     bytes in flight, appended chunk by chunk.
<root>/rec/<NAME>.json.tmp      a sidecar mid-write; never matched by a HEAD.
```

`<NAME>` is the camera's own segment name (`YYYYMMDD-HHMMSS.h264`, from
`rec.rs`) and the directory is `rec/`, so a copy of the tree looks the same on
both ends. A half-received file cannot be mistaken for a complete one: the
segment name never ends in `.part`, and the rename that creates it happens only
after the sha256 of the stored bytes matched.

## What the handlers do

| request | answer |
|---|---|
| `HEAD PREFIX/NAME` | `200 X-Stored-Sha256: <hex> X-Stored-Bytes: <n>`, or `404` |
| `HEAD PREFIX/NAME.part` | `200 X-Upload-Offset: <n> X-Stored-Bytes: <n>`, or `404` |
| `PUT PREFIX/NAME.part` + `Content-Range`, `Content-SHA256` | `200 X-Upload-Offset: <first+n>`; `409 X-Upload-Offset: <what we hold>` if `first` is not our offset; `400` for a bad chunk; `413` over the cap |
| `POST PREFIX/NAME.finalize` + `X-Upload-Total`, `X-Upload-Sha256` | `200 X-Stored-Sha256: <what we hashed>`; `409 X-Upload-Offset` on a total mismatch; `422 X-Stored-Sha256` on a sha mismatch (the `.part` is kept); `404` with nothing held |
| `PUT PREFIX/NAME.json` + `Content-SHA256` | `200 X-Stored-Sha256` |
| `DELETE PREFIX/NAME.part` | `200 X-Upload-Offset: 0` (idempotent) |

`HEAD` answers always carry `Content-Length: 0` and no body. That is not
pedantry — see "what would have bitten the camera" below.

## Decisions where `upload.rs` left a choice

Every one of these is the stricter option, and every one was re-checked against
the real client (see "verification").

1. **`Content-SHA256` is mandatory on a chunk and on a sidecar.** The camera
   always sends it. Accepting a body without it would mean writing bytes we
   cannot check into a file whose commit is then decided by the same
   unauthenticated link.
2. **A chunk's body is buffered and verified before it is appended.** So a
   short or corrupt chunk leaves the `.part` exactly as it was and the client's
   resume point stays honest. Cost: at most `--max-chunk` (8 MiB) of RAM per
   connection, and the connection cap bounds that.
3. **`Content-Length` must equal the length `Content-Range` implies** (`last -
   first + 1`), and `total` must be strictly greater than `last`. A request that
   disagrees with itself is a 400, not a guess.
4. **`Content-Range` is parsed digit by digit** (`bytes FIRST-LAST/TOTAL`), so
   `+0`, `0x10`, `1e3` and `*/300` are all 400s.
5. **A 409 never repeats the offset the client just used.** The client treats
   "409 with the same offset" as a protocol failure and gives up on the file, so
   a conflict can only be answered when the offsets really differ.
6. **Commit hashes the file it stored and echoes that**, never the header. On a
   mismatch it keeps the `.part` and answers `422` with `X-Stored-Sha256` set to
   the truth, so the camera's log says what actually landed.
7. **Commit is idempotent without a `.part`.** If the segment is already there
   with the same length and the same sha, that is a `200` (a lost reply), not a
   `404`.
8. **Names are validated, not sanitised.** `[A-Za-z0-9._-]`, no leading `.`, no
   `..`, ≤ 200 chars, single path component, percent-decoded first and then
   judged, printable ASCII only. Anything else is a 400 and never reaches a
   path. A path outside the prefix is a 404 (`/ingestfoo/x` included — a naive
   `starts_with` would serve it).
9. **`Transfer-Encoding: chunked` is refused with 411.** A chunked body would
   have to be read to be measured, and this endpoint may face the internet.
10. **`Expect: 100-continue` is answered** with the interim `HTTP/1.1 100
    Continue` before the body is read, so curl and proxies do not sit out their
    one-second timeout. The camera never sends it.
11. **The token may also arrive as `Authorization: Bearer <t>`**, for curl. It is
    compared in constant time (length folded in), the presented value is never
    logged, and a missing token is 401 while a wrong one is 403 — the camera
    fails both the same way, but an operator needs to tell them apart.
12. **`.json` sidecars are written to `<NAME>.json.tmp` and renamed**, and the
    segment rename is followed by an `fsync` of the directory, so a power cut
    cannot leave a complete-looking file with half the bytes.
13. **One request per connection, then close** — the camera sends `Connection:
    close` on every request, and the response says the same. It works behind a
    reverse proxy; it is not keep-alive.

## What it deliberately does not do

- **No TLS.** The protocol assumes plain HTTP on a private path (`upload.rs`:
  "Plain HTTP over a WireGuard tunnel is the v1 assumption"), and `upload.rs`
  has since grown a TLS *client* (`src/tls.rs`, mbedTLS). If the camera is to
  reach this over the public internet, terminate TLS in front (Caddy on
  `edge-proxy-01` → this server) and keep `X-Cam-Token` as the authorisation —
  the protocol's own note applies: the token travels in cleartext to whatever
  terminates TLS, and that hop should be a private network.
- **No retention.** Disk fills at ~10.5 GB/day for 720p at 0.97 Mbit/s (see
  `work/offsite-2026-11-14.md`), i.e. ~54 days on the 570 GB free on
  storage-01. Pruning is an operator job; nothing here deletes a segment.
- **No `GET`/playback, no listing, no index.** The receiver is a sink.

## Verification

Two layers:

1. **26 tests in `src/tests.rs`**, driving the real server over a real loopback
   socket with a hand-rolled client that matches `upload.rs` header for header:
   happy path (video + sidecar + idempotent commit), resume after a dead
   connection, a chunk retried at the same offset (409, and the `.part` does not
   grow), out-of-order chunks, a client that dies mid-body (nothing is written),
   a `.part` longer than the file plus the `DELETE` reset, a bad chunk sha
   (nothing written), a commit whose declared sha is wrong (422, real sha echoed,
   `.part` kept), a bad sidecar sha, missing/wrong/empty tokens, 18 traversal and
   prefix-escape targets, the empty-prefix deployment, oversized and
   self-contradictory bodies, `Expect: 100-continue`, chunked bodies, junk on the
   socket, two simultaneous retries of one chunk (exactly one 200 and one 409),
   two names in parallel, plus unit tests for SHA-256 vectors, path resolution,
   prefix normalisation and `Content-Range`.

   ```sh
   rustc --test --edition 2021 -C opt-level=1 src/main.rs -o /tmp/camingest-tests
   /tmp/camingest-tests --quiet
   ```

2. **The camera's own uploader against this server.** `upload.rs` was compiled
   unmodified as a module in a throwaway harness (TLS stubbed out; the real
   `tls.rs` links mbedTLS objects that only the camera build has) and pointed at
   a live `camingest` over loopback. Five scenarios, all passing: a fresh
   1 MiB segment (bytes identical on both ends, sidecar stored, local pair
   deleted only after the echo matched); the same segment again (answered by
   handler 1: `already_there`, 81 bytes on the wire, local pair deleted);
   a resume from a pre-seeded 40 % `.part` (`resumed_from 419430`, 629 KB sent,
   bytes identical); a `.part` longer than the file (`DELETE`, restart, bytes
   identical); and a wrong token (`403`, nothing stored, nothing deleted).

## Operational notes

- The camera's `base_url` must match `--prefix`. `http://192.168.1.240:8080/ingest`
  with the default prefix, and note that a `base_url` with no path makes the
  camera ask for `/NAME` — use `--prefix ""` for that.
- The store directory is created at startup (`<root>/rec`). A full disk shows up
  as a 500 on the chunk that could not be written; the client retries with
  backoff and keeps its copy, which is the correct failure.
- 503 means "over the connection cap", not "broken": the client treats 5xx as
  retryable.
- Logs go to stdout, one line per event, UTC timestamps; run it under systemd
  and let the journal do the rest.
