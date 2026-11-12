// The drawing half of the page, in a worker on an OffscreenCanvas (as in muse-rs): the sample
// buffers, autoscaling and every canvas call live here, so streaming never waits on drawing.
//
// Messages in: {canvas}, {size: {width, height, ratio}}, {series: {index, tag, name, unit}},
// {samples: {index, t, v}} (typed arrays, a frame's worth), {window: seconds}, {paused},
// {grouped}, {dark}, {reset}.

let canvas = null, g = null;
let size = { width: 800, height: 600, ratio: 1 };
let windowS = 10, paused = false, pausedAt = 0, grouped = true, dark = false;

// Per series: tag, name, unit, and samples as parallel arrays (time ms, value).
const series = new Map(); // index -> { tag, name, unit, t: [], v: [] }
let latest = 0; // newest sample time, ms
const CAP = 12000;

// Display order and grouping, by stock tag. Battery and SD fill are in the header.
const ORDER = ['EA', 'EL', 'PI', 'PR', 'PG', 'HR', 'BI', 'ACC', 'GYRO', 'MAG', 'T1', 'TH', 'T0', 'H0', 'SCR'];
const GROUP_OF = { AX: 'ACC', AY: 'ACC', AZ: 'ACC', GX: 'GYRO', GY: 'GYRO', GZ: 'GYRO', MX: 'MAG', MY: 'MAG', MZ: 'MAG', SA: 'SCR', SR: 'SCR', SF: 'SCR' };
const GROUP_NAME = { ACC: 'Accelerometer', GYRO: 'Gyroscope', MAG: 'Magnetometer', SCR: 'SCR (amplitude, rise time, per minute)' };
const HIDDEN = new Set(['BV', 'B%', 'SD']);
// Event-like channels: drawn as steps held to the present.
const STEPS = new Set(['HR', 'BI', 'SA', 'SR', 'SF']);
const COLORS = ['#2f6fde', '#e5484d', '#1c9a55', '#d8890b', '#8e4ec6', '#0090a8'];
const AXIS_COLOR = { X: '#e5484d', Y: '#1c9a55', Z: '#2f6fde', A: '#e5484d', R: '#1c9a55', F: '#2f6fde' };

function rank(key) {
  const i = ORDER.indexOf(key);
  return i < 0 ? ORDER.length : i;
}

function rows() {
  const byKey = new Map();
  for (const [index, s] of series) {
    if (HIDDEN.has(s.tag) || s.t.length === 0) continue;
    const key = grouped && GROUP_OF[s.tag] ? GROUP_OF[s.tag] : s.tag;
    if (!byKey.has(key)) byKey.set(key, []);
    byKey.get(key).push(index);
  }
  return [...byKey.entries()].sort((a, b) => rank(a[0]) - rank(b[0]) || a[0].localeCompare(b[0]));
}

function fmt(v) {
  if (!Number.isFinite(v)) return '–';
  const a = Math.abs(v);
  return a >= 1000 ? v.toFixed(0) : a >= 10 ? v.toFixed(1) : a >= 1 ? v.toFixed(2) : v.toFixed(4);
}

function draw() {
  requestAnimationFrame(draw);
  if (!g) return;
  const { width: w, height: h, ratio } = size;
  g.setTransform(ratio, 0, 0, ratio, 0, 0);
  g.clearRect(0, 0, w, h);
  const list = rows();
  if (list.length === 0) {
    g.fillStyle = '#8b93a1'; g.font = '14px system-ui';
    g.fillText('Waiting for data…', 16, 28);
    return;
  }
  const cols = w > 900 ? 2 : 1;
  const rowsN = Math.ceil(list.length / cols);
  const cellW = w / cols, cellH = Math.max(60, h / rowsN);
  const now = paused ? pausedAt : latest;
  const t0 = now - windowS * 1000;
  list.forEach(([key, members], k) => {
    const x0 = (k % cols) * cellW, y0 = Math.floor(k / cols) * cellH;
    const padL = 58, padR = 10, padT = 22, padB = 14;
    const pw = cellW - padL - padR, ph = cellH - padT - padB;
    // Range over the visible window.
    let lo = Infinity, hi = -Infinity;
    for (const i of members) {
      const s = series.get(i);
      for (let j = s.t.length - 1; j >= 0 && s.t[j] >= t0; j--) {
        if (s.t[j] > now) continue;
        lo = Math.min(lo, s.v[j]); hi = Math.max(hi, s.v[j]);
      }
      // Held steps: the last value before the window counts too.
      if (STEPS.has(s.tag) && s.t.length) { const last = s.v[s.v.length - 1]; lo = Math.min(lo, last); hi = Math.max(hi, last); }
    }
    if (!Number.isFinite(lo)) { lo = -1; hi = 1; }
    if (hi - lo < 1e-9 * Math.max(1, Math.abs(hi))) { const m = (hi + lo) / 2; lo = m - 1; hi = m + 1; }
    const pad = (hi - lo) * 0.08; lo -= pad; hi += pad;
    const X = (t) => x0 + padL + pw * ((t - t0) / (windowS * 1000));
    const Y = (v) => y0 + padT + ph * (1 - (v - lo) / (hi - lo));
    // Frame and ticks.
    g.strokeStyle = 'rgba(128,128,128,0.25)'; g.lineWidth = 1;
    g.fillStyle = '#8b93a1'; g.font = '10px ui-monospace, monospace';
    for (let q = 0; q <= 2; q++) {
      const v = hi - (hi - lo) * q / 2, y = Y(v);
      g.beginPath(); g.moveTo(x0 + padL, y); g.lineTo(x0 + padL + pw, y); g.stroke();
      g.fillText(fmt(v), x0 + 4, y + 3);
    }
    // Traces.
    members.forEach((i, m) => {
      const s = series.get(i);
      const axis = GROUP_OF[s.tag] ? s.tag.slice(-1) : null;
      g.strokeStyle = axis && grouped ? AXIS_COLOR[axis] : COLORS[m % COLORS.length];
      g.lineWidth = 1.4; g.beginPath();
      const step = STEPS.has(s.tag);
      let started = false, lastY = 0, j = s.t.length - 1;
      while (j > 0 && s.t[j - 1] >= t0) j--;
      if (step && j > 0) j--; // start from the value held into the window
      for (; j < s.t.length; j++) {
        if (s.t[j] > now) break;
        const x = Math.max(X(s.t[j]), x0 + padL), y = Y(s.v[j]);
        if (!started) { g.moveTo(x, y); started = true; }
        else if (step) { g.lineTo(x, lastY); g.lineTo(x, y); }
        else g.lineTo(x, y);
        lastY = y;
      }
      if (started && step) g.lineTo(X(now), lastY);
      g.stroke();
    });
    // Title and latest values.
    const first = series.get(members[0]);
    const title = members.length > 1 ? (GROUP_NAME[GROUP_OF[first.tag]] ?? first.name) : first.name;
    g.fillStyle = dark ? '#e7e9ee' : '#14171c'; g.font = '600 12px system-ui';
    g.fillText(title, x0 + padL, y0 + 14);
    g.font = '11px ui-monospace, monospace'; g.textAlign = 'right';
    const values = members.map((i) => {
      const s = series.get(i);
      return `${members.length > 1 ? s.tag.slice(-1).toLowerCase() + ' ' : ''}${fmt(s.v[s.v.length - 1])}`;
    }).join('  ') + (first.unit ? ` ${first.unit}` : '');
    g.fillStyle = '#6b7280'; g.fillText(values, x0 + cellW - padR, y0 + 14);
    g.textAlign = 'left';
  });
}

onmessage = ({ data: m }) => {
  if (m.canvas) { canvas = m.canvas; g = canvas.getContext('2d'); requestAnimationFrame(draw); }
  if (m.size) {
    size = m.size;
    if (canvas) { canvas.width = Math.round(size.width * size.ratio); canvas.height = Math.round(size.height * size.ratio); }
  }
  if (m.reset) { series.clear(); latest = 0; }
  if (m.series) series.set(m.series.index, { ...m.series, t: [], v: [] });
  if (m.samples) {
    const { index, t, v } = m.samples;
    for (let k = 0; k < index.length; k++) {
      const s = series.get(index[k]);
      if (!s) continue;
      s.t.push(t[k]); s.v.push(v[k]);
      if (s.t.length > CAP) { s.t.splice(0, s.t.length - CAP); s.v.splice(0, s.v.length - CAP); }
      if (t[k] > latest) latest = t[k];
    }
  }
  if (m.window) windowS = m.window;
  if (m.paused !== undefined) { paused = m.paused; pausedAt = latest; }
  if (m.grouped !== undefined) grouped = m.grouped;
  if (m.dark !== undefined) dark = m.dark;
};
