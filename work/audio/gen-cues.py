#!/usr/bin/env python3
"""gen-cues.py -- build the camera's notification cues at chosen levels.

Loudness is baked into the file: the stock decoder hardcodes DAC 6 / DEV 5 /
ASLC 6 (maximum gain) and takes no volume argument, so peak amplitude is the
only lever we have.  Levels below were chosen by ear on 2026-10-24:

    cue    -40 dBFS   comfortable, everyday notification  (user: "3 was best")
    warn   -30 dBFS   attention, still not painful        (user: "2 was still loud")
    alarm  -20 dBFS   reserve for emergency/critical only (user: "1 ... emergency")

Output: mono 16 kHz signed 16-bit little-endian, no WAV header (ak_adec_demo
type "pcm").  Play with quiet-cue.sh so the active-low amp is actually enabled.
"""
import array
import math
import os
import sys

RATE = 16000
FADE = 0.005          # 5 ms ramps: no click at the edges

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

def build(parts, dbfs, path):
    peak = 32768.0 * (10 ** (dbfs / 20.0))
    buf = array.array("h")
    for kind, val, ms in parts:
        buf += gap(ms) if kind == "gap" else tone(val, ms, peak)
    with open(path, "wb") as f:
        buf.tofile(f)
    return max(abs(v) for v in buf), len(buf) / RATE

# cue: rising two-tone "I am up" -- 880 Hz then 1174 Hz (D6)
CUE = [("tone", 880, 120), ("gap", 0, 60), ("tone", 1174, 180)]
# warn: three even pips
WARN = [("tone", 880, 90), ("gap", 0, 80), ("tone", 880, 90),
        ("gap", 0, 80), ("tone", 880, 90)]
# alarm: urgent alternation, no gaps between the pairs
ALARM = [("tone", 660, 140), ("gap", 0, 60), ("tone", 990, 140),
         ("gap", 0, 60), ("tone", 660, 140), ("gap", 0, 60),
         ("tone", 990, 140)]

TARGETS = [("cue.pcm", CUE, -40.0), ("warn.pcm", WARN, -30.0),
           ("alarm.pcm", ALARM, -20.0)]

for name, parts, dbfs in TARGETS:
    p = os.path.join(os.path.dirname(os.path.abspath(__file__)), name)
    peak, dur = build(parts, dbfs, p)
    real = 20 * math.log10(peak / 32768.0)
    print("%-10s %6d bytes  %4.2f s  peak %5d  %+.1f dBFS  sha256 %s"
          % (name, peak and os.path.getsize(p), dur, peak, real,
             __import__("hashlib").sha256(open(p, "rb").read()).hexdigest()[:16]))
