#!/usr/bin/env python3
"""Summarizes scripts/power-run.sh: per run, the battery voltage's slope (least squares over the
host's BV samples, after the first SKIP seconds while the voltage settles to the new load), start
and end voltage and percent, records and drops.

    scripts/power-report.py OUT_DIR [SKIP_SECONDS=120]

A LiPo's voltage does not fall evenly with charge, so one run per profile says little. With
alternating rounds (ROUNDS in power-run.sh), the comparison below fits
    slope = profile + a smooth trend over time (the discharge curve)
and reports each profile's drain relative to the first one, with a standard error. The slope is a
relative measure of current, not a current."""
import csv
import glob
import os
import re
import sys


def samples(path):
    out = []
    with open(path, newline="") as f:
        for row in csv.reader(f):
            try:
                out.append((float(row[0]), float(row[-1])))
            except (ValueError, IndexError):
                continue  # header
    return out


def slope(points):
    n = len(points)
    if n < 3:
        return None
    mx = sum(t for t, _ in points) / n
    my = sum(v for _, v in points) / n
    sxx = sum((t - mx) ** 2 for t, _ in points)
    return sum((t - mx) * (v - my) for t, v in points) / sxx if sxx else None


def solve(a, b):
    """Least squares for a @ x ~= b: x and the covariance factor (A^T A)^-1."""
    p = len(a[0])
    ata = [[sum(r[i] * r[j] for r in a) for j in range(p)] for i in range(p)]
    atb = [sum(r[i] * y for r, y in zip(a, b)) for i in range(p)]
    # Gauss-Jordan on [ata | I].
    m = [row[:] + [1.0 if i == j else 0.0 for j in range(p)] for i, row in enumerate(ata)]
    for c in range(p):
        piv = max(range(c, p), key=lambda r: abs(m[r][c]))
        if abs(m[piv][c]) < 1e-12:
            return None, None
        m[c], m[piv] = m[piv], m[c]
        d = m[c][c]
        m[c] = [v / d for v in m[c]]
        for r in range(p):
            if r != c and m[r][c]:
                f = m[r][c]
                m[r] = [v - f * w for v, w in zip(m[r], m[c])]
    inv = [row[p:] for row in m]
    return [sum(inv[i][j] * atb[j] for j in range(p)) for i in range(p)], inv


def compare(blocks):
    """blocks: (profile, mid time in hours, slope in mV/h). Prints each profile's drain against
    the first profile's, the discharge curve fitted out."""
    profiles = list(dict.fromkeys(p for p, _, _ in blocks))
    if len(profiles) < 2 or len(blocks) < len(profiles) + 3:
        return
    t0 = sum(t for _, t, _ in blocks) / len(blocks)
    rows = [[1.0 if p == q else 0.0 for q in profiles] + [t - t0, (t - t0) ** 2] for p, t, _ in blocks]
    y = [s for _, _, s in blocks]
    x, inv = solve(rows, y)
    if x is None:
        return
    resid = [yi - sum(r * xi for r, xi in zip(row, x)) for row, yi in zip(rows, y)]
    dof = len(y) - len(x)
    var = sum(e * e for e in resid) / dof if dof > 0 else float("nan")
    base = profiles[0]
    print(f"\nComparison ({len(blocks)} blocks; slope = profile + trend over time; residual sd {var ** 0.5:.0f} mV/h):")
    print(f"  {'profile':16} {'blocks':>6} {'mV/h':>8} {'vs ' + base:>14} {'± se':>8}")
    for i, p in enumerate(profiles):
        n = sum(1 for q, _, _ in blocks if q == p)
        if i == 0:
            print(f"  {p:16} {n:>6} {x[i]:>8.0f} {'':>14} {(var * inv[i][i]) ** 0.5:>8.0f}")
            continue
        se = (var * (inv[i][i] + inv[0][0] - 2 * inv[i][0])) ** 0.5
        ratio = x[i] / x[0] if x[0] else float("nan")
        print(f"  {p:16} {n:>6} {x[i]:>8.0f} {f'{ratio:.2f}x ({x[i] - x[0]:+.0f})':>14} {se:>8.0f}")
    print("  Lower (less negative) mV/h is less drain; a difference under about 2 se is not shown to be real.")


def main(out, skip=120.0):
    rows = []
    blocks = []
    first_t = None
    for run in sorted(glob.glob(os.path.join(out, "*-*")), key=os.path.getmtime):
        if not os.path.isdir(run):
            continue
        name = os.path.basename(run)
        bv = sorted(p for f in glob.glob(os.path.join(run, "host", "*_BV.csv")) for p in samples(f))
        if bv:
            bv = [p for p in bv if p[0] >= bv[0][0] + skip] or bv
        pct = sorted(p for f in glob.glob(os.path.join(run, "host", "*_B%.csv")) for p in samples(f))
        stream = open(os.path.join(run, "stream.txt"), errors="replace").read() if os.path.exists(os.path.join(run, "stream.txt")) else ""
        rec = re.search(r"(\d+) records in ([\d.]+) s \(([\d.]+)/s\), (\d+) lost", stream)
        drop = re.search(r"(\d+) dropped in firmware", stream)
        s = slope(bv)
        if s is not None and bv:
            first_t = bv[0][0] if first_t is None else first_t
            profile = re.sub(r"^r\d+-", "", name).rsplit("-", 1)[0]
            blocks.append((profile, ((bv[0][0] + bv[-1][0]) / 2 - first_t) / 3600, s * 3600 * 1000))
        rows.append((
            name,
            f"{len(bv)}",
            f"{bv[0][1]:.3f}" if bv else "-",
            f"{bv[-1][1]:.3f}" if bv else "-",
            f"{s * 3600 * 1000:+.0f}" if s is not None else "-",
            f"{pct[0][1]:.0f}->{pct[-1][1]:.0f}" if pct else "-",
            f"{rec.group(1)} @ {rec.group(3)}/s, {rec.group(4)} lost" if rec else "-",
            drop.group(1) if drop else "-",
        ))
    head = ("run", "BV n", "V start", "V end", "mV/h", "battery %", "records", "dropped")
    widths = [max(len(str(r[i])) for r in rows + [head]) for i in range(len(head))]
    for r in [head] + rows:
        print("  ".join(str(c).ljust(w) for c, w in zip(r, widths)))
    compare(blocks)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else ".", float(sys.argv[2]) if len(sys.argv) > 2 else 120.0)
