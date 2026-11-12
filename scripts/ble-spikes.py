#!/usr/bin/env python3
"""Count isolated one-sample spikes per channel in an `emotibit ble --record-dir` folder
(or the output of `emotibit parse`).

A spike: the sample differs from both neighbours by more than 25x the channel's median
absolute first difference, while the neighbours agree with each other."""
import csv, glob, statistics as st, sys

d = sys.argv[1]
total = 0
for f in sorted(glob.glob(f"{d}/*.csv")):
    tag = f.rsplit("_", 1)[1][:-4]
    if tag in ("HR", "BI", "SA", "SR", "SF", "B%", "BV"):
        continue
    try:
        # The value is the last column (per-channel CSVs from `parse` and `--record-dir`).
        v = [float(r[-1]) for r in list(csv.reader(open(f)))[1:] if r]
    except ValueError:
        continue  # not a numeric channel (time sync, notes, ...)
    if len(v) < 10:
        continue
    diffs = [abs(b - a) for a, b in zip(v, v[1:])]
    scale = max(st.median(diffs), 1e-3)
    spikes = []
    for i in range(1, len(v) - 1):
        a, x, b = v[i - 1], v[i], v[i + 1]
        if abs(x - a) > 25 * scale and abs(x - b) > 25 * scale and abs(a - b) < 5 * scale:
            spikes.append((i, round(a, 3), round(x, 3), round(b, 3)))
    total += len(spikes)
    print(f"{tag:3} n={len(v):6} spikes={len(spikes):3} {spikes[:3]}")
print("total", total)
