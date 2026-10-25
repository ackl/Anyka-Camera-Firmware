#!/usr/bin/env python3
"""measure-tone.py RAW_F s16le RATE FREQ [WINDOW_MS]

Per-window level of a raw mono s16le stream, plus the amplitude of one
frequency measured with a Goertzel filter in each window.  Used to answer
"did the speaker actually make a sound" with a number taken from the camera's
own microphone instead of a pair of ears across the room.

    ffmpeg -v error -i cap.mkv -ac 1 -ar 8000 -f s16le cap.raw
    measure-tone.py cap.raw 8000 1000 250

Prints: t_start  rms_dBFS  tone_dBFS  (rms over the window, tone = amplitude
of FREQ as dBFS).  A silent window sits near the noise floor; a playing tone
lifts the tone column well above the rms column.
"""
import array
import math
import sys


def goertzel_amp(samples, freq, rate):
    n = len(samples)
    if n < 2:
        return 0.0
    coeff = 2.0 * math.cos(2.0 * math.pi * freq / rate)
    s1 = s2 = 0.0
    for x in samples:
        s0 = x + coeff * s1 - s2
        s2, s1 = s1, s0
    power = s1 * s1 + s2 * s2 - coeff * s1 * s2
    # normalise: amplitude of a sine that fills the window
    return math.sqrt(max(power, 0.0)) / n * 2.0


def db(x, full=32768.0):
    return 20.0 * math.log10(max(x, 1e-9) / full)


def main():
    if len(sys.argv) < 4:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    path, rate, freq = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
    win_ms = int(sys.argv[4]) if len(sys.argv) > 4 else 250

    a = array.array("h")
    with open(path, "rb") as fh:
        a.frombytes(fh.read())
    if sys.byteorder == "big":
        a.byteswap()

    n = max(1, int(rate * win_ms / 1000))
    print("t_start\trms_dBFS\ttone_dBFS")
    best = (-999.0, 0.0)
    for start in range(0, len(a) - n + 1, n):
        win = a[start:start + n]
        rms = math.sqrt(sum(float(x) * x for x in win) / len(win))
        amp = goertzel_amp([float(x) for x in win], freq, rate)
        t = start / rate
        if amp > 0:
            tone_db = db(amp)
            if tone_db > best[0]:
                best = (tone_db, t)
        else:
            tone_db = -180.0
        print("%.2f\t%.1f\t%.1f" % (t, db(rms), tone_db))
    print("# peak tone %.1f dBFS at t=%.2fs (%d windows of %d ms, %g Hz)"
          % (best[0], best[1], len(a) // n, win_ms, freq))
    return 0


if __name__ == "__main__":
    sys.exit(main())
