// The page half of the browser version (as in muse-rs): this thread holds Web Bluetooth and
// the wasm module, because `navigator.bluetooth` is not available to workers and
// `requestDevice` needs a user gesture; `render.js` does the drawing on an OffscreenCanvas.
//
// `webbluetooth.js` is the shim from `webbluetooth-wasm`, copied in by `build.sh`, so it
// matches the module's ABI.
import { instantiate } from './webbluetooth.js';

const $ = (id) => document.getElementById(id);
const canvas = $('traces');
let instance = null;
let worker = null;
const decoder = new TextDecoder(), encoder = new TextEncoder();

function text(ptr, len) {
  return decoder.decode(new Uint8Array(instance.exports.memory.buffer, ptr, len));
}

/// Passes a string to an export taking (ptr, len); the module frees it.
function pass(fn, s) {
  const bytes = encoder.encode(s);
  const ptr = instance.exports.eb_alloc(bytes.length);
  new Uint8Array(instance.exports.memory.buffer, ptr, bytes.length).set(bytes);
  fn(ptr, bytes.length);
}
const command = (...fields) => pass(instance.exports.eb_command, fields.join('\t'));

function banner(msg, ms = 6000) {
  const b = $('banner');
  b.hidden = !msg; b.textContent = msg ?? '';
  clearTimeout(banner.t);
  if (msg && ms) banner.t = setTimeout(() => { b.hidden = true; }, ms);
}

// ── Samples, batched to the worker once a frame ─────────────────────────────────
const BATCH = 4096;
const sIndex = new Uint32Array(BATCH), sTime = new Float64Array(BATCH), sValue = new Float64Array(BATCH);
let pending = 0, received = 0, receivedAtTick = 0;

function flush() {
  if (pending === 0) return;
  worker.postMessage({ samples: { index: sIndex.slice(0, pending), t: sTime.slice(0, pending), v: sValue.slice(0, pending) } });
  pending = 0;
}

// ── A stock packet line from the board: `ts,num,count,TAG,1,100,payload...` ─────
function fields(line) {
  const f = line.split(',');
  return { tag: f[3], payload: f.slice(6) };
}

let clip = 0, over = 0;
function onMessage(line) {
  const { tag, payload } = fields(line);
  const keyed = (k) => { const i = payload.indexOf(k); return i >= 0 ? payload[i + 1] ?? '' : undefined; };
  switch (tag) {
    case 'CG': {
      const serial = (keyed('device_id') ?? '').split('-').pop();
      const name = keyed('name') ?? '';
      $('set-name').value = name === serial ? '' : name;
      $('set-ppg').value = keyed('ppg_hz') ?? '25';
      $('set-leds').checked = keyed('leds') === '1';
      const from = { default: 'defaults', flash: 'flash', card: "the SD card's CONFIG.TXT (it wins at boot)" }[keyed('source')] ?? keyed('source');
      $('set-info').textContent = `${keyed('device_id') || 'no device id'} · settings from ${from}` +
        (keyed('restart_needed') === '1' ? ' · the new name shows after a restart' : '');
      break;
    }
    case 'CS': case 'CW': case 'CC':
      if (payload[0] === 'error') $('set-info').textContent = `Error: ${payload.slice(1).join(', ')}`;
      else if (payload[1]) banner(`${tag === 'CW' ? 'SD card: ' : ''}${payload[1]}`, 4000);
      break;
    case 'ST': {
      const t = $('selftest-table'); t.replaceChildren();
      for (let i = 0; i + 1 < payload.length; i += 2) {
        const tr = t.insertRow();
        tr.insertCell().textContent = payload[i];
        const c = tr.insertCell(); c.textContent = payload[i + 1];
        c.className = payload[i + 1] === 'PASS' ? 'pass' : payload[i + 1] === 'FAIL' ? 'fail' : 'muted';
      }
      break;
    }
    case 'FS':
      // A row (empty trailing fields may be missing), or the bare end marker.
      if (payload.length > 0) sessions.push(payload);
      else { showSessions(); command('FL'); }
      break;
    case 'FL':
      if (payload.length >= 2) files.push([payload[0], Number(payload[1])]);
      else showFiles();
      break;
    case 'FR': case 'FY': case 'FZ': case 'FX': {
      // `<tag>,<name>,ok,<new>` / `<tag>,<name>,error,<why>` / `FY,<name>,copying,<done>,<total>`;
      // `FX,<name>,deleted|<why>`.
      const [name, result, a = '', b = ''] = payload;
      if (result === 'copying') { $('files-info').textContent = `Copying ${name}: ${kB(Number(a))} of ${kB(Number(b))}…`; break; }
      const ok = result === 'ok' || result === 'deleted';
      const what = { FR: `Renamed to ${a}`, FY: `Copied to ${a}`, FZ: 'Deleted', FX: 'Deleted' }[tag];
      banner(ok ? `${name}: ${what}.` : `${name}: ${tag === 'FX' ? result : a}`, ok ? 4000 : 8000);
      busy = false;
      refreshFiles();
      break;
    }
    case 'DC': case 'DO':
      for (let i = 1; i < payload.length; i += 2) { const n = Number(payload[i]) || 0; if (tag === 'DC') clip += n; else over += n; }
      $('clip-chip').hidden = false; $('clip').textContent = `${clip}/${over}`;
      $('clip-chip').classList.add('bad');
      break;
    case 'BS': banner('Button pressed on the board.', 3000); break;
    case 'BL': banner('Button held on the board: it hibernates.', 0); break;
  }
}

let files = [], sessions = [], busy = false;
function refreshFiles() {
  sessions = []; files = [];
  $('files-info').textContent = 'Reading the card…';
  $('sessions-table').replaceChildren(); $('files-table').replaceChildren();
  command('FS');
}
// Rename, duplicate or delete: one at a time, answered by an `FR`/`FY`/`FZ`/`FX` line.
function fileOp(tag, ...args) {
  if (busy) { banner('The card is busy with the last change.', 3000); return; }
  busy = true;
  $('files-info').textContent = { FR: 'Renaming…', FY: 'Copying…', FZ: 'Deleting…', FX: 'Deleting…' }[tag];
  command(tag, ...args);
}
// The name the board accepts for `FR`/`FY`: letters, digits, space, - _ . ( ), up to 36.
const NAME_OK = /^[A-Za-z0-9_()-](?:[A-Za-z0-9 _.()-]{0,34}[A-Za-z0-9_()-])?$/;
function askName(title, initial) {
  for (let v = initial; ;) {
    v = prompt(`${title}\nLetters, digits, space, - _ . ( ), up to 36 characters; the extension stays.`, v);
    if (v === null) return null;
    v = v.trim().replace(/\.(csv|ebb)$/i, '');
    if (NAME_OK.test(v)) return v;
    alert(`"${v}" cannot be used as a name.`);
  }
}
const stem = (name) => name.replace(/\.(csv|ebb)$/i, '');
const STAMP = /^(\d{4}-\d{2}-\d{2})_(\d{2})-(\d{2})-(\d{2})/;
// The start: the info file's `created_at` (it survives a rename), else a stock file name's time.
function startOf(row) {
  const m = (row[7] ?? '').match(STAMP) ?? row[0].match(STAMP);
  return m ? `${m[1]} ${m[2]}:${m[3]}:${m[4]}` : '';
}
const kB = (b) => (b >= 1e6 ? `${(b / 1e6).toFixed(1)} MB` : `${(b / 1e3).toFixed(0)} kB`);
function download(name) {
  $('files-info').textContent = `Downloading ${name}…`;
  pass(instance.exports.eb_sd_get, name);
}

// `FS` rows: data file, format, state, duration (s), bytes, notes, markers, created.
function showSessions() {
  const t = $('sessions-table'); t.replaceChildren();
  const head = t.createTHead().insertRow();
  for (const h of ['Start', 'Name', 'Duration', 'Size', 'Format', 'Notes', 'Markers', 'State', '']) { const th = document.createElement('th'); th.textContent = h; head.append(th); }
  // Newest first; undated ones (older firmware's 8.3 names) after the dated ones.
  sessions.sort((a, b) => startOf(b).localeCompare(startOf(a)) || b[0].localeCompare(a[0]));
  const body = t.createTBody();
  for (const row of sessions) {
    const [name, format, state, dur = '', bytes = '0', notes = '', markers = ''] = row;
    const r = body.insertRow();
    const cell = (v, cls) => { const c = r.insertCell(); c.textContent = v; if (cls) c.className = cls; return c; };
    cell(startOf(row) || '–');
    cell(stem(name), 'mono').title = name;
    const s = Number(dur);
    cell(dur === '' ? '–' : s >= 3600 ? `${Math.floor(s / 3600)}:${String(Math.floor(s / 60) % 60).padStart(2, '0')}:${String(Math.round(s) % 60).padStart(2, '0')}` : `${Math.floor(s / 60)}:${String(Math.round(s) % 60).padStart(2, '0')}`, 'n');
    cell(kB(Number(bytes)), 'n');
    cell(format);
    cell(notes || '–', 'n'); cell(markers || '–', 'n');
    cell(state, `state-${state}`);
    const actions = r.insertCell(); actions.className = 'actions';
    const button = (label, title, onclick) => {
      const b = document.createElement('button'); b.textContent = label; b.title = title;
      // Nothing touches the file being written.
      b.disabled = state === 'recording'; b.onclick = onclick;
      actions.append(b, ' ');
      return b;
    };
    button('Data', `Download ${name}`, () => download(name));
    // The info file goes with the data file; only dated (stock-named) recordings had one.
    if (row[7] || STAMP.test(name)) {
      const info = `${stem(name)}_info.json`;
      button('Info', `Download ${info}`, () => download(info));
    }
    button('Rename…', 'Rename the recording (data and info file)', () => {
      const to = askName(`Rename ${name} to:`, stem(name));
      if (to !== null && to !== stem(name)) fileOp('FR', name, to);
    });
    button('Duplicate', 'Copy the recording on the card', () => {
      const to = askName(`Copy ${name} as:`, `${stem(name)} copy`);
      if (to !== null) fileOp('FY', name, to);
    });
    button('Delete', 'Delete the recording (data and info file)', () => {
      if (confirm(`Delete ${name} and its info file from the SD card? This cannot be undone.`)) fileOp('FZ', name);
    }).classList.add('danger');
  }
  const total = sessions.reduce((a, r) => a + Number(r[4] || 0), 0);
  $('files-info').textContent = sessions.length
    ? `${sessions.length} recordings, ${kB(total)} · downloads run at about 5 kB/s over Bluetooth, copies at about 300 kB/s on the card; neither while recording · convert with emotibit parse`
    : 'No recordings on the card.';
}

function showFiles() {
  const t = $('files-table'); t.replaceChildren();
  const inSessions = new Set(sessions.map((r) => r[0]));
  for (const [name, size] of files.filter(([n]) => !inSessions.has(n) && !/_info\.json$/.test(n))) {
    const tr = t.insertRow();
    tr.insertCell().textContent = name;
    tr.insertCell().textContent = kB(size);
    const b = document.createElement('button'); b.textContent = 'Download';
    // Settings and credentials stay on the card.
    b.disabled = /^CONFIG\./i.test(name);
    b.onclick = () => download(name);
    const x = document.createElement('button'); x.textContent = 'Delete'; x.className = 'danger';
    x.disabled = /^CONFIG\.TXT$/i.test(name);
    x.onclick = () => { if (confirm(`Delete ${name} from the SD card? This cannot be undone.`)) fileOp('FX', name); };
    tr.insertCell().append(b, ' ', x);
  }
}

// ── The module's imports ─────────────────────────────────────────────────────────
let state = {};
const imports = {
  emotibit: {
    eb_now_ms: () => Date.now(),
    eb_tz_offset_min: () => new Date().getTimezoneOffset(),
    eb_status(p, l) { $('status').textContent = text(p, l); },
    eb_log(level, p, l) {
      const method = [, 'error', 'warn', 'info', 'debug', 'debug'][level] || 'log';
      console[method](`[emotibit] ${text(p, l)}`);
    },
    eb_device(p, l) {
      const name = l ? text(p, l) : '';
      $('device').textContent = name || 'not connected';
      $('dot').className = `dot ${name ? 'live' : ''}`;
      $('toolbar').hidden = !name;
      if (!name) for (const id of ['settings', 'selftest', 'files']) $(id).hidden = true;
    },
    eb_series(index, tp, tl, np, nl, up, ul) {
      worker.postMessage({ series: { index, tag: text(tp, tl), name: text(np, nl), unit: text(up, ul) } });
    },
    eb_sample(index, t, v) {
      received += 1;
      if (pending === BATCH) flush();
      sIndex[pending] = index; sTime[pending] = t; sValue[pending] = v; pending += 1;
    },
    eb_state(p, l) {
      state = JSON.parse(text(p, l));
      $('battery').textContent = state.battery_percent != null ? `${state.battery_percent.toFixed(0)}%` : '–';
      const rec = !!state.recording;
      $('rec-chip').hidden = !rec; $('rec-text').textContent = `SD ${state.recording_file ?? ''}`;
      $('record').textContent = rec ? '■ Stop SD recording' : '● Record on SD';
      $('record').classList.toggle('on', rec);
      const mode = { Normal: 'MN', Low: 'ML', MaxLow: 'MM', WirelessOff: 'MO', Hibernate: 'MH' }[state.power_mode];
      if (mode && document.activeElement !== $('power')) $('power').value = mode;
      // Link: dropped in firmware, MTU, batch, bytes, lost on the way, received.
      const k = state.link ?? [];
      if (k.length >= 6) {
        $('link-chip').hidden = false;
        $('link').textContent = `${state.compact ? 'compact' : 'raw'} · MTU ${k[1]}${k[0] ? ` · ${k[0]} dropped` : ''}`;
        $('lost').textContent = k[4]; $('lost-chip').classList.toggle('bad', k[4] > 0);
      }
    },
    eb_message(p, l) { onMessage(text(p, l)); },
    eb_progress(have, total) { $('files-info').textContent = `Downloading… ${(have / 1000).toFixed(0)} / ${(total / 1000).toFixed(0)} kB`; },
    eb_file(np, nl, p, l) {
      const name = text(np, nl);
      const bytes = new Uint8Array(instance.exports.memory.buffer, p, l).slice();
      const a = document.createElement('a');
      a.href = URL.createObjectURL(new Blob([bytes])); a.download = name; a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 10000);
      $('files-info').textContent = `Saved ${name} (${(l / 1000).toFixed(1)} kB). Convert it with emotibit parse.`;
    },
    eb_active(on) {
      $('connect').disabled = on === 1;
      $('disconnect').hidden = on !== 1;
      if (on === 1) { worker.postMessage({ reset: true }); $('intro').hidden = true; canvas.hidden = false; postSize(); clip = over = 0; $('clip-chip').hidden = true; }
      else { $('dot').className = 'dot'; $('link-chip').hidden = true; $('rec-chip').hidden = true; }
    },
  },
};

// ── Wiring ──────────────────────────────────────────────────────────────────────
function postSize() {
  const b = canvas.getBoundingClientRect();
  worker.postMessage({ size: { width: b.width, height: b.height, ratio: devicePixelRatio || 1 } });
}

const WINDOWS = [2, 5, 10, 20, 30, 60];
let win = 2, paused = false;
function setWindow(i) {
  win = Math.max(0, Math.min(WINDOWS.length - 1, i));
  $('win').textContent = `${WINDOWS[win]} s`;
  worker.postMessage({ window: WINDOWS[win] });
}

if (!navigator.bluetooth) {
  $('status').textContent = 'This browser has no Web Bluetooth: use Chrome or Edge, over localhost or HTTPS.';
} else if (!canvas.transferControlToOffscreen) {
  $('status').textContent = 'This browser cannot hand a canvas to a worker (no OffscreenCanvas).';
} else {
  worker = new Worker('./render.js');
  const offscreen = canvas.transferControlToOffscreen();
  worker.postMessage({ canvas: offscreen }, [offscreen]);
  new ResizeObserver(postSize).observe(canvas);
  const dark = matchMedia('(prefers-color-scheme: dark)');
  const postTheme = () => worker.postMessage({ dark: dark.matches });
  dark.addEventListener('change', postTheme); postTheme();
  setWindow(win);

  instance = await instantiate(fetch('./web.wasm'), imports);

  const pump = () => { flush(); requestAnimationFrame(pump); };
  requestAnimationFrame(pump);
  setInterval(() => {
    const per = received - receivedAtTick; receivedAtTick = received;
    $('rate').textContent = received ? `${per}/s` : '–';
    $('foot').textContent = `${received} samples`;
  }, 1000);

  const fmt0 = localStorage.getItem('emotibit-web-format');
  if (fmt0) $('format').value = fmt0;
  $('connect').disabled = false;
  $('status').textContent = 'Ready.';
  // Straight to the module inside the click: the chooser needs the user gesture.
  $('connect').onclick = () => { localStorage.setItem('emotibit-web-format', $('format').value); instance.exports.eb_connect(Number($('format').value)); };
  $('disconnect').onclick = () => instance.exports.eb_disconnect();
  $('record').onclick = () => state.recording ? command('RE') : command('RB', $('binary').checked ? 'bin' : 'csv');
  $('note-btn').onclick = () => { const t = $('note').value.trim(); if (t) { command('UN', t); $('note').value = ''; banner('Note added to the recording.', 2500); } };
  $('note').onkeydown = (e) => { if (e.key === 'Enter') $('note-btn').click(); };
  $('power').onchange = (e) => {
    const tag = e.target.value;
    if ((tag === 'MO' || tag === 'MH') && !confirm(tag === 'MH' ? 'Hibernate powers the board down until its button is pressed. Continue?' : 'The board turns its radio off until its button is pressed. Continue?')) return;
    command(tag);
  };
  $('win-').onclick = () => setWindow(win - 1);
  $('win+').onclick = () => setWindow(win + 1);
  $('pause').onclick = () => { paused = !paused; $('pause').textContent = paused ? 'Resume' : 'Pause'; worker.postMessage({ paused }); };
  $('grouped').onchange = (e) => worker.postMessage({ grouped: e.target.checked });
  const toggle = (id, open) => { const p = $(id); p.hidden = !p.hidden; if (!p.hidden) open(); };
  $('settings-btn').onclick = () => toggle('settings', () => { $('set-info').textContent = 'Reading…'; command('CG'); });
  $('selftest-btn').onclick = () => toggle('selftest', () => { $('selftest-table').replaceChildren(); command('ST'); });
  // Sessions first (FS), then the full file list (FL) for the rest.
  $('files-btn').onclick = () => toggle('files', refreshFiles);
  const save = (restart) => {
    const name = $('set-name').value.trim();
    if (!/^[A-Za-z0-9_-]{0,9}$/.test(name)) { $('set-info').textContent = 'Name: up to 9 letters, digits, - or _ (empty: the serial number).'; return; }
    command('CS', 'name', name, 'ppg_hz', $('set-ppg').value, 'leds', $('set-leds').checked ? '1' : '0', 'store', $('set-store').value);
    if (restart) { command('CR'); banner('Saved; the board restarts. Connect again when it is back.', 8000); }
    else setTimeout(() => command('CG'), 600);
  };
  $('set-save').onclick = () => save(false);
  $('set-restart').onclick = () => save(true);
  addEventListener('keydown', (e) => {
    if (['INPUT', 'SELECT'].includes(e.target.tagName) || $('toolbar').hidden) return;
    if (e.key === 'ArrowLeft') setWindow(win - 1);
    if (e.key === 'ArrowRight') setWindow(win + 1);
    if (e.key === ' ') { e.preventDefault(); $('pause').click(); }
  });
}
