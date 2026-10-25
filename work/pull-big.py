#!/usr/bin/env python3
"""pull-big.py REMOTE LOCAL EXPECTED_SHA256 [PORT] -- stream a large file off the board.

`tools/get` is fine for small files, but it reads the whole blob into memory
with a 40 s socket timeout, so a ~26 MB transfer never completes: every attempt
is discarded and retried. This streams straight into LOCAL.part with no size
cap, then compares sha256 with the board's own hash and only renames on match.

Same two traps as tools/get: the board listener takes a second to appear (never
probe it -- a probe consumes the connection), and a stalled telnet session can
leave nothing listening. So each attempt starts a fresh `nc -l -p PORT`, and a
wrong hash means the .part is deleted and the whole thing retried.

usage: work/pull-big.py REMOTE LOCAL EXPECTED_SHA256 [PORT]
env: CAM_HOST (default 192.168.1.103)
"""

import hashlib
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
CAM = os.path.join(os.path.dirname(HERE), "tools", "cam")
HOST = os.environ.get("CAM_HOST", "192.168.1.103")
ATTEMPTS = 4


def main():
    if len(sys.argv) < 4:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    remote, local, want = sys.argv[1], sys.argv[2], sys.argv[3].lower()
    port = int(sys.argv[4]) if len(sys.argv) > 4 else 9988
    part = local + f".part.{os.getpid()}"  # pid-unique: a stale run cleans up its own

    for attempt in range(1, ATTEMPTS + 1):
        if os.path.exists(part):
            os.remove(part)
        # CAM_TIMEOUT is 30 s by default, which SEGFAULTS the transfer at 30 s
        # (exactly what a 26 MB pull looks like: ~18-20 MB, then a clean EOF).
        # The listener must outlive the whole stream.
        env = dict(os.environ, CAM_TIMEOUT="600")
        listener = subprocess.Popen(
            [CAM, f"nc -l -p {port} < {remote}"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, env=env)
        got = 0
        t0 = time.time()
        try:
            time.sleep(4)
            s = socket.create_connection((HOST, port), timeout=180)
            s.settimeout(180)
            h = hashlib.sha256()
            with open(part, "wb") as f:
                while True:
                    chunk = s.recv(65536)
                    if not chunk:
                        break
                    f.write(chunk)
                    h.update(chunk)
                    got += len(chunk)
            s.close()
        except OSError as e:
            print(f"  attempt {attempt}: socket error after {got} bytes: {e}",
                  flush=True)
            h = None
        finally:
            listener.terminate()
            try:
                listener.wait(timeout=5)
            except subprocess.TimeoutExpired:
                listener.kill()
        dt = time.time() - t0
        digest = h.hexdigest() if h and got else None
        rate = got / dt / 1024 if dt else 0
        print(f"  attempt {attempt}: {got} bytes in {dt:.1f}s "
              f"({rate:.0f} KB/s), sha256 {digest or 'nothing'}", flush=True)
        if digest == want:
            os.replace(part, local)
            print(f"verified: {local} ({got} bytes, attempt {attempt})", flush=True)
            return 0

    if os.path.exists(part):
        os.remove(part)
    print(f"FAILED to retrieve {remote}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
