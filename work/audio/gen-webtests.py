#!/usr/bin/env python3
"""gen-webtests.py -- assets for the web UI speaker test panel (2026-10-25).

The user swapped the speaker, which voids every earlier ear verdict, so the web
panel needs a level ladder it can play on demand.  Two kinds of file:

  wt-NN.pcm    three 1 kHz clicks, 0.15 s on / 0.25 s off, at -NN dBFS.
               Short on purpose: the user asked for 0.2 s test tones
               ("2s too long for test tones. do like 0.2s").
  wt-long.pcm  3 s continuous 1 kHz at -20 dBFS -- "is this speaker wired and
               powered at all", with no timing to judge.

Same convention as gen-cues.py: mono 16 kHz s16le, no WAV header, 5 ms fades,
peak = 32768 * 10^(dBFS/20).  Loudness lives in the file because the decoder
hardcodes DAC 6 / DEV 5 / ASLC 6 and takes no volume argument.
"""
import array
import hashlib
import math
import os

RATE = 16000
FADE = 0.005
HERE = os.path.dirname(os.path.abspath(__file__))


def tone(freq, ms, peak):
    n = int(RATE * ms / 1000)
    fade = min(int(RATE * FADE), n // 2)
    out = array.array("h")
    for i in range(n):
        v = math.sin(2 * math.pi * freq * i / RATE)
        if i < fade:
            v *= i / fade
        elif i > n - fade - 1:
            v *= max(0.0, (n - 1 - i) / fade)
        out.append(int(round(v * peak)))
    return out


def gap(ms):
    return array.array("h", bytes(2 * int(RATE * ms / 1000)))


def save(buf, path):
    with open(path, "wb") as f:
        buf.tofile(f)
    return max(abs(v) for v in buf), len(buf) / RATE


def clicks(dbfs):
    peak = 32768.0 * (10 ** (dbfs / 20.0))
    buf = array.array("h")
    for _ in range(3):
        buf += tone(1000, 150, peak)
        buf += gap(250)
    return buf


targets = []
for lvl in (40, 30, 20, 10):
    targets.append(("wt-%d.pcm" % lvl, clicks(-float(lvl)), -float(lvl)))

long_peak = 32768.0 * (10 ** (-20.0 / 20.0))
targets.append(("wt-long.pcm", tone(1000, 3000, long_peak), -20.0))

for name, buf, dbfs in targets:
    path = os.path.join(HERE, name)
    peak, dur = save(buf, path)
    real = 20 * math.log10(peak / 32768.0) if peak else float("-inf")
    sha = hashlib.sha256(open(path, "rb").read()).hexdigest()
    print("%-11s %6d B  %4.2f s  peak %5d  %+.1f dBFS  sha256 %s"
          % (name, os.path.getsize(path), dur, peak, real, sha[:16]))
