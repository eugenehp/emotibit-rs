// Browser oscilloscope. All decoding, timing, periodizing, status and scaling is done by the
// Rust core compiled to wasm (WebScope); this file moves bytes in and draws.
//
// Data sources, so the page is useful with or without a device:
//   ble     Web Bluetooth, straight from a board running the custom emotibit-ble firmware
//   bridge  live packets from `emotibit serve` over a WebSocket (stock firmware on WiFi)
//   serial  Web Serial, one signal from the stock firmware's debug mode
//   demo    synthetic EmotiBit-like packets generated here
//   replay  a raw recording (.csv from the SD card, or saved from this page)
import init, { WebScope } from './pkg/emotibit.js';

await init();
let scope = new WebScope();
const $ = (id) => document.getElementById(id);

// ── Channels and plot layout ─────────────────────────────────────────────────────
const COLORS = {
  PR: '#e5484d', PI: '#8e4ec6', PG: '#12a594', HR: '#e5484d', BI: '#e5484d',
  EA: '#2f6fde', EL: '#2f6fde', ER: '#2f6fde',
  T0: '#d8890b', T1: '#d8890b', TH: '#e5484d', H0: '#12a594',
  BV: '#1c9a55', 'B%': '#1c9a55',
};
const AXIS = ['#f76b15', '#1c9a55', '#2f6fde']; // x, y, z
const GROUPS = { ACC: ['AX', 'AY', 'AZ'], GYRO: ['GX', 'GY', 'GZ'], MAG: ['MX', 'MY', 'MZ'], SCR: ['SA', 'SR', 'SF'] };
const GROUP_NAME = { ACC: 'Accelerometer', GYRO: 'Gyroscope', MAG: 'Magnetometer', SCR: 'Skin conductance responses' };
// Shown in the header instead of a plot.
const HIDDEN = new Set(['B%']);
const GROUP_OF = Object.fromEntries(Object.entries(GROUPS).flatMap(([g, ts]) => ts.map((t) => [t, g])));
// Display order: physiology first, then temperatures, motion, battery; anything else after.
const ORDER = ['EA', 'EL', 'ER', 'SCR', 'SA', 'SR', 'SF', 'PI', 'PR', 'PG', 'HR', 'BI', 'O2', 'TH', 'T1', 'T0', 'H0',
  'ACC', 'AX', 'AY', 'AZ', 'GYRO', 'GX', 'GY', 'GZ', 'MAG', 'MX', 'MY', 'MZ', 'BV', 'B%'];
const rank = (k) => { const i = ORDER.indexOf(k); return i < 0 ? ORDER.length : i; };
const color = (tag) => COLORS[tag] ?? (GROUP_OF[tag] ? AXIS[GROUPS[GROUP_OF[tag]].indexOf(tag)] : '#8b93a1');

// ── Source state ─────────────────────────────────────────────────────────────────
let source = 'none';        // 'ble' | 'bridge' | 'serial' | 'demo' | 'replay' | 'none'
let ws = null;
let timer = null;           // demo / replay scheduler
let lastPacketAt = 0;
let retry = null;
const SOURCE_LABEL = { ble: 'Bluetooth', bridge: 'WiFi', serial: 'USB serial', demo: 'demo', replay: 'recording' };

function resetScope() {
  scope.free();
  scope = new WebScope();
  for (const { div } of plots.values()) div.remove();
  plots.clear();
  rate.last = null; rate.hist = null; rate.bps = null;
  $('pause').textContent = 'Pause';
}

function stopSource() {
  saveRecording();
  stopSerial();
  stopBle();
  clearInterval(timer); timer = null;
  clearTimeout(retry); retry = null;
  if (ws) { ws.onclose = null; ws.close(); ws = null; }
  source = 'none';
  $('speedwrap').hidden = true;
}

/// Connection state shown in the header: `live`, `wait` or `bad`, plus a short text.
function setState(kind, text) {
  $('dot').className = `dot ${kind ?? ''}`;
  $('state').textContent = text ?? '';
}

/// Switch between the start screen and the scope, and show the controls that apply.
function showScope(on) {
  $('start').hidden = on;
  $('toolbar').hidden = !on;
  $('change').hidden = !on || source === 'none';
  $('disconnect').hidden = !on || !['ble', 'bridge', 'serial'].includes(source);
  const badge = $('src-badge');
  badge.hidden = !on; badge.textContent = SOURCE_LABEL[source] ?? '';
  const bridge = source === 'bridge';
  // Notes and power modes: the bridge, or a BLE board that takes commands.
  const control = bridge || (source === 'ble' && !!ble.control);
  for (const id of ['note', 'note-btn', 'powerwrap']) $(id).hidden = !control;
  $('sd-btn').hidden = $('sd-bin-wrap').hidden = $('settings-btn').hidden = !(source === 'ble' && ble.control);
  if (!(source === 'ble' && ble.control)) $('settings').hidden = true;
  $('clip-chip').hidden = !bridge && source !== 'replay';
  $('link-chip').hidden = source !== 'ble';
  $('rec-btn').title = bridge ? 'Record to the device’s SD card' : 'Record in the browser; saved as a raw .csv when you stop';
  $('empty').hidden = !on || plots.size > 0;
  if (!on) { $('device').textContent = 'not connected'; setState('', ''); }
}

function banner(text, ms, kind = 'warn') {
  const b = $('banner'); clearTimeout(banner.t);
  b.style.display = text ? 'block' : 'none'; b.textContent = text ?? '';
  b.className = kind === 'info' ? 'info' : '';
  if (text && ms) banner.t = setTimeout(() => banner(null), ms);
}

/// Host local time in the stock `TL` format (`2026-11-07_18-56-52-837506`).
function localStamp(d = new Date()) {
  const p = (n, w = 2) => String(n).padStart(w, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}_${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}-${p(d.getMilliseconds() * 1000, 6)}`;
}

// ── Recording in the browser (every source but the bridge, which records on the SD card) ──
function startRecording() {
  // The stamp anchors the file so `emotibit parse` can give wall-clock times.
  const d = new Date();
  scope.record_start(['ble', 'serial'].includes(source) ? localStamp(d) : undefined);
  recStarted = d;
}
let recStarted = null;
function saveRecording() {
  if (!scope.is_recording()) return;
  const text = scope.record_stop();
  if (!text) { banner('Nothing was recorded.', 4000, 'info'); return; }
  const d = recStarted ?? new Date(), p = (n) => String(n).padStart(2, '0');
  const name = `emotibit-${source === 'none' ? 'rec' : source}-${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}_${p(d.getHours())}-${p(d.getMinutes())}-${p(d.getSeconds())}.csv`;
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([text], { type: 'text/csv' }));
  a.download = name; a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 10000);
  banner(`Saved ${name} (${text.split('\n').length - 1} packets). Open it here to replay, or split it with \`emotibit parse\`.`, 8000, 'info');
}

// ── Bridge ───────────────────────────────────────────────────────────────────────
/// `keep`: a retry after the bridge went away; keeps the plots.
function connectBridge(url, keep = false) {
  if (!keep) { stopSource(); resetScope(); }
  source = 'bridge'; showScope(true);
  localStorage.setItem('emotibit-bridge', url);
  setState('wait', 'connecting…'); $('device').textContent = url;
  try { ws = new WebSocket(url); } catch (e) { banner(`Bad bridge URL: ${e.message}`); setState('bad', ''); return; }
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.type === 'hello') { $('device').textContent = `${m.device} (${m.ip})`; setState('live', ''); banner(null); }
    else if (m.type === 'packets') { scope.push_lines(m.lines.join('\n'), Date.now()); lastPacketAt = performance.now(); }
    else if (m.type === 'disconnected') { banner('The device disconnected from the bridge.'); setState('bad', 'device lost'); }
    else if (m.type === 'ack' && !m.ok) banner(`${m.cmd}: ${m.error}`, 4000);
  };
  ws.onclose = () => {
    if (source !== 'bridge') return;
    banner(`Cannot reach the bridge at ${url}. Is \`emotibit serve\` running? Retrying…`);
    setState('wait', 'retrying…');
    ws = null; retry = setTimeout(() => { if (source === 'bridge') connectBridge(url, true); }, 2000);
  };
}
function send(obj) { if (source === 'bridge' && ws?.readyState === 1) ws.send(JSON.stringify(obj)); }

// ── Demo: synthetic packets with the stock firmware's tags, rates and batching ────
function startDemo() {
  stopSource(); resetScope();
  source = 'demo'; showScope(true);
  setState('live', 'synthetic data'); $('device').textContent = 'Demo device'; banner(null);
  // One packet-number sequence for the whole device, like the real firmware.
  let seq = 0, dev = 100000, batch = 0;
  const wall0 = Date.now();
  const num = (v) => (typeof v === 'number' ? +v.toFixed(4) : v);
  const line = (tag, vals) => `${dev},${(seq++) & 0xffff},${vals.length},${tag},1,100,${vals.map(num).join(',')}`;
  const wave = (f, a, c, k = 3) => Array.from({ length: k }, (_, i) => c + a * Math.sin(2 * Math.PI * f * (dev - (k - 1 - i) * 40) / 1000));
  timer = setInterval(() => {
    dev += 120; batch++;
    const L = [];
    L.push(line('PG', wave(1.2, 120, 600)), line('PI', wave(1.2, 200, 3100)), line('PR', wave(1.2, 220, 3700)));
    L.push(line('AX', wave(0.3, 0.05, -0.19)), line('AY', wave(0.2, 0.03, 0.09)), line('AZ', wave(0.1, 0.02, 1.07)));
    L.push(line('GX', wave(0.5, 0.6, 0)), line('GY', wave(0.4, 0.5, 0)), line('GZ', wave(0.3, 0.4, 0)));
    L.push(line('MX', wave(0.05, 8, -30)), line('MY', wave(0.05, 6, 12)), line('MZ', wave(0.05, 5, -40)));
    L.push(line('EA', wave(0.02, 0.004, 0.035, 2)));
    if (batch % 2 === 0) L.push(line('T1', [32.4 + 0.05 * Math.sin(dev / 5000)]), line('TH', [30.4 + 0.05 * Math.cos(dev / 5000)]));
    if (batch % 8 === 0) L.push(line('HR', [72 + 3 * Math.sin(dev / 9000)]), line('EM', ['RS', 'RE', 'PS', 'MN']));
    if (batch % 40 === 0) L.push(line('B%', [100 - Math.floor(batch / 400)]), line('BV', [4.2]));
    scope.push_lines(L.join('\n'), wall0 + (dev - 100000)); lastPacketAt = performance.now();
  }, 120);
}

// ── Replay: a raw recording, packets delivered on the device's own clock ────────
function startReplay(text, name) {
  const rows = [];
  for (const l of text.split('\n')) {
    const line = l.trim(); if (!line) continue;
    const ts = Number(line.slice(0, line.indexOf(',')));
    if (Number.isFinite(ts)) rows.push({ ts, line });
  }
  if (!rows.length) { banner('No EmotiBit packets found in that file.', 5000); return; }
  stopSource(); resetScope();
  // Keep file order (arrival order): different sensors stamp the same packet group slightly
  // differently, so sorting by time would scramble the packet-number sequence. Pace by the
  // running maximum of the device clock instead.
  let run = rows[0].ts;
  for (const r of rows) { run = Math.max(run, r.ts); r.at = run; }
  source = 'replay'; showScope(true); $('speedwrap').hidden = false;
  const t0 = rows[0].ts, wall0 = Date.now(), tEnd = run;
  $('device').textContent = name; banner(null);
  setState('live', `replaying ${rows.length} packets, ${((tEnd - t0) / 1000).toFixed(0)} s`);
  let i = 0, started = performance.now(), base = 0; // base: data ms already played
  timer = setInterval(() => {
    const speed = Number($('speed').value);
    const now = performance.now();
    if (!scope.is_paused()) base += (now - started) * speed;
    started = now;
    const due = [];
    while (i < rows.length && rows[i].at - t0 <= base) due.push(rows[i++].line);
    if (due.length) {
      // Virtual arrival time = wall start + device time, so spacing is right at any speed.
      scope.push_lines(due.join('\n'), wall0 + (rows[i - 1].at - t0)); lastPacketAt = performance.now();
    }
    if (i >= rows.length) { clearInterval(timer); timer = null; setState('', `replay finished (${((tEnd - t0) / 1000).toFixed(0)} s)`); }
  }, 30);
}

// ── USB serial (Web Serial): one signal, straight from the Feather, no WiFi and no bridge ───────
// The stock firmware prints raw values in a debug mode that must be entered right after a reset,
// and its serial input is unreliable on some boots, so a handshake that stalls is retried with
// another reboot (same policy as `emotibit serial`). Handshake logic lives in Rust (wasm).
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let serialStop = null;
function logLine(text) {
  const el = $('serial-log'); el.hidden = false;
  const lines = (el.textContent + '\n' + text).split('\n').slice(-60);
  el.textContent = lines.join('\n').replace(/^\n/, ''); el.scrollTop = el.scrollHeight;
}
function stopSerial() {
  if (serialStop) serialStop();
  serialStop = null;
  $('serial-log').hidden = true; $('serial-log').textContent = '';
}

async function startSerial() {
  let port;
  try { port = await navigator.serial.requestPort({ filters: [{ usbVendorId: 0x10c4 }] }); } // CP210x (ESP32 Feather)
  catch { return; } // cancelled or no matching device
  try { await port.open({ baudRate: 2000000 }); }
  catch (e) { banner(`Could not open the port (is another program using it?): ${e.message}`, 8000); return; }

  stopSource(); resetScope();
  source = 'serial'; showScope(true);
  const sig = $('sig').value;
  $('device').textContent = `Feather · ${sig}`;
  banner('Resetting the board; the stream starts after it reboots (about 30 s).', 8000, 'info');
  const enc = new TextEncoder();
  const writer = port.writable.getWriter();
  const reader = port.readable.pipeThrough(new TextDecoderStream()).getReader();
  let closed = false, cycles = 1, lastReset = performance.now(), debugSince = null;
  const MAX_CYCLES = 4, PATIENCE = 45000;

  const act = (json) => {
    for (const a of JSON.parse(json)) {
      if (a.send) writer.write(enc.encode(a.send)).catch(() => {});
      else if (a.state) {
        if (a.state === 'entering-debug' && debugSince == null) debugSince = performance.now();
        setState(a.state === 'streaming' ? 'live' : 'wait', a.state.replace('-', ' '));
      } else if (a.log) logLine(a.log);
    }
  };
  const reset = async () => {
    await port.setSignals({ dataTerminalReady: false, requestToSend: true });
    await sleep(100);
    await port.setSignals({ requestToSend: false });
    lastReset = performance.now(); debugSince = null;
    scope.serial_start(sig);
  };

  scope.serial_start(sig);
  setState('wait', 'waiting for boot');
  await reset();

  const supervisor = setInterval(async () => {
    const now = performance.now();
    act(scope.serial_tick(now));
    const state = scope.serial_state();
    const stalled = (state === 'waiting' && now - lastReset > 12000) ||
                    ((state === 'entering-debug' || state === 'starting') && debugSince != null && now - debugSince > PATIENCE);
    if (stalled && !closed) {
      if (cycles >= MAX_CYCLES) {
        banner('No data after 4 reboot attempts. The firmware’s debug input is unreliable on some boots; try again.', 12000);
        stopSource(); setState('bad', 'gave up'); return;
      }
      cycles++; banner(`Stream did not start; rebooting (attempt ${cycles} of ${MAX_CYCLES})…`, 6000, 'info');
      await reset();
    }
  }, 250);

  serialStop = () => {
    closed = true; clearInterval(supervisor);
    reader.cancel().catch(() => {});
    try { writer.releaseLock(); } catch {}
    setTimeout(() => port.close().catch(() => {}), 150);
  };

  (async () => {
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done || closed) break;
        act(scope.serial_feed(value, performance.now()));
        lastPacketAt = performance.now();
      }
    } catch (e) { if (!closed) banner(`Serial read stopped: ${e.message}`, 8000); }
  })();
}

// ── Bluetooth (Web Bluetooth): the custom emotibit-ble firmware, no WiFi and no bridge ─────────
// A dropped link is retried a few times, keeping the plots and the recording.
const ble = { dev: null, onDisconnect: null, compact: true, lostBefore: 0, attempts: 0, wanted: false, control: null, writes: Promise.resolve() };
const BLE_RETRIES = 5;

function stopBle() {
  ble.wanted = false;
  if (ble.dev) {
    ble.dev.removeEventListener('gattserverdisconnected', ble.onDisconnect);
    if (ble.dev.gatt.connected) ble.dev.gatt.disconnect();
  }
  ble.dev = null; ble.control = null;
}

/// Sends a stock control line to the board (one write at a time).
function bleSend(tag, payload = []) {
  const ctl = ble.control; if (!ctl) return;
  const line = scope.ble_command(tag, payload, Date.now());
  ble.writes = ble.writes.then(() => ctl.writeValueWithResponse(new TextEncoder().encode(line))).catch((e) => banner(`Command ${tag} failed: ${e.message}`, 4000));
}

/// Connect GATT, select the wire format and subscribe. Throws on failure.
async function bleSubscribe(dev) {
  const server = await dev.gatt.connect();
  const service = await server.getPrimaryService(WebScope.ble_service_uuid());
  const ch = await service.getCharacteristic(WebScope.ble_data_uuid());
  // Firmware without the mode characteristic only speaks raw.
  let mode = null;
  try { mode = await service.getCharacteristic(WebScope.ble_mode_uuid()); } catch { /* old firmware */ }
  const compact = ble.compact && !!mode;
  if (mode) await mode.writeValueWithResponse(Uint8Array.of(compact ? 1 : 0));
  // Keep the loss count across reconnects (ble_start resets the session counters).
  ble.lostBefore += scope.ble_counts()[1];
  scope.ble_start(compact);
  ch.addEventListener('characteristicvaluechanged', (e) => {
    const v = e.target.value;
    scope.ble_feed(new Uint8Array(v.buffer, v.byteOffset, v.byteLength), Date.now());
    lastPacketAt = performance.now();
  });
  await ch.startNotifications();
  // Commands and status (newer firmware): SD recording, notes, power modes, time sync.
  ble.control = null;
  try {
    const events = await service.getCharacteristic(WebScope.ble_events_uuid());
    ble.control = await service.getCharacteristic(WebScope.ble_control_uuid());
    const dec = new TextDecoder();
    events.addEventListener('characteristicvaluechanged', (e) => {
      const v = e.target.value;
      // Answer time-sync requests, so the board's SD files get wall-clock anchors.
      for (const reply of scope.ble_events(dec.decode(v), Date.now(), localStamp())) {
        const ctl = ble.control;
        ble.writes = ble.writes.then(() => ctl?.writeValueWithResponse(new TextEncoder().encode(reply))).catch(() => {});
      }
    });
    await events.startNotifications();
  } catch { /* firmware without commands */ }
  showScope(true);
  // Restart the encoder now that notifications are flowing, so it begins with key frames.
  if (mode) await mode.writeValueWithResponse(Uint8Array.of(compact ? 1 : 0));
  return compact;
}

async function startBle() {
  ble.compact = document.querySelector('input[name=fmt]:checked').value === 'compact';
  localStorage.setItem('emotibit-ble-format', ble.compact ? 'compact' : 'raw');
  let dev;
  try {
    dev = await navigator.bluetooth.requestDevice({
      filters: [{ namePrefix: WebScope.ble_device_name() }],
      optionalServices: [WebScope.ble_service_uuid()],
    });
  } catch { return; } // chooser cancelled, or nothing found
  stopSource(); resetScope();
  source = 'ble'; showScope(true);
  ble.dev = dev; ble.lostBefore = 0; ble.attempts = 0; ble.wanted = true;
  $('device').textContent = dev.name ?? 'EmotiBit-BLE';
  setState('wait', 'connecting…');
  ble.onDisconnect = () => { if (source === 'ble' && ble.wanted) bleReconnect(); };
  dev.addEventListener('gattserverdisconnected', ble.onDisconnect);
  try {
    const compact = await bleSubscribe(dev);
    ble.format = compact ? 'compact' : 'raw';
    setState('live', ''); banner(null);
  } catch (e) {
    banner(`Bluetooth connection failed: ${e.message}`, 8000);
    setState('bad', 'failed'); stopBle();
  }
}

async function bleReconnect() {
  while (source === 'ble' && ble.wanted && ble.attempts < BLE_RETRIES) {
    ble.attempts++;
    setState('wait', `reconnecting (${ble.attempts}/${BLE_RETRIES})…`);
    await sleep(1000 * 2 ** (ble.attempts - 1));
    if (source !== 'ble' || !ble.wanted) return;
    try {
      await bleSubscribe(ble.dev);
      ble.attempts = 0; setState('live', ''); banner('Bluetooth reconnected.', 4000, 'info');
      return;
    } catch { /* try again */ }
  }
  if (source === 'ble' && ble.wanted) {
    setState('bad', 'disconnected');
    banner('Bluetooth link lost. Is the board on and in range? Use “Change source” to connect again.');
  }
}

async function replayFromUrl(url) {
  try {
    const r = await fetch(url);
    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
    startReplay(await r.text(), url.split('/').pop() || url);
  } catch (e) { banner(`Could not load ${url}: ${e.message}`, 6000); }
}

// ── Controls ─────────────────────────────────────────────────────────────────────
let sdRecording = false;
$('connect').onclick = () => connectBridge($('bridge').value.trim() || 'ws://127.0.0.1:8765/');
$('bridge').onkeydown = (e) => { if (e.key === 'Enter') $('connect').click(); };
$('demo').onclick = startDemo;
for (const n of WebScope.serial_signals()) { const o = document.createElement('option'); o.value = o.textContent = n; $('sig').append(o); }
$('serial').onclick = startSerial;
$('ble').onclick = startBle;
const fmt0 = localStorage.getItem('emotibit-ble-format');
if (fmt0) for (const r of document.querySelectorAll('input[name=fmt]')) r.checked = r.value === fmt0;
const insecure = !isSecureContext ? ' This page must be served over https:// or from localhost.' : '';
if (!('bluetooth' in navigator)) { $('ble').disabled = true; $('ble-why').hidden = false; $('ble-why').textContent = `Web Bluetooth is not available in this browser (use Chrome or Edge).${insecure}`; }
if (!('serial' in navigator)) { $('serial').disabled = true; $('serial-why').hidden = false; $('serial-why').textContent = `Web Serial is not available in this browser (use Chrome or Edge).${insecure}`; }
$('load').onclick = () => $('file').click();
$('file').onchange = async (e) => { const f = e.target.files[0]; if (f) startReplay(await f.text(), f.name); e.target.value = ''; };
addEventListener('dragover', (e) => { e.preventDefault(); document.body.classList.add('dropping'); });
addEventListener('dragleave', (e) => { if (!e.relatedTarget) document.body.classList.remove('dropping'); });
addEventListener('drop', async (e) => {
  e.preventDefault(); document.body.classList.remove('dropping');
  const f = e.dataTransfer.files[0]; if (f) startReplay(await f.text(), f.name);
});
$('change').onclick = () => { stopSource(); resetScope(); showScope(false); banner(null); };
$('disconnect').onclick = () => { stopSource(); setState('', 'disconnected'); $('disconnect').hidden = true; };
$('sd-btn').onclick = () => {
  if (sdRecording) bleSend('RE', [localStamp()]);
  else bleSend('RB', $('sd-bin').checked ? [localStamp(), 'FMT', 'BIN'] : [localStamp()]);
};
// Board settings (BLE): read with CG, change with CS, restart with CR.
$('settings-btn').onclick = () => {
  const panel = $('settings');
  panel.hidden = !panel.hidden;
  if (!panel.hidden) { $('set-info').textContent = 'Reading the board’s settings…'; lastSettings = ''; bleSend('CG'); }
};
let lastSettings = '';
// Numeric settings in the panel (ids set-<key>), and the board's last answer.
const NUMS = ['led_brightness', 'cpu_mhz', 'sensor_ms', 'conn_ms', 'adv_ms', 'tx_dbm', 'ppg_led'];
let shown = {};
function saveSettings(restart) {
  const name = $('set-name').value.trim();
  if (!/^[A-Za-z0-9_-]{0,9}$/.test(name)) { $('set-info').textContent = 'Name: up to 9 letters, digits, - or _ (empty: the serial number).'; return; }
  // A profile picked here sets everything; otherwise only what was changed is sent, so the
  // board keeps matching its profile where it can.
  const args = ['name', name];
  const profile = $('set-profile').value;
  if (profile !== 'custom' && profile !== shown.profile) args.push('profile', profile);
  else {
    const leds = $('set-leds').checked ? '1' : '0';
    if ($('set-ppg').value !== shown.ppg_hz) args.push('ppg_hz', $('set-ppg').value);
    if (leds !== shown.leds) args.push('leds', leds);
    for (const k of NUMS) {
      const v = $(`set-${k}`).value;
      if (shown[k] !== undefined && v !== '' && v !== shown[k]) args.push(k, v);
    }
  }
  bleSend('CS', [...args, 'store', $('set-store').value]);
  if (restart) { bleSend('CR'); $('set-info').textContent = 'Saved; the board restarts and the page reconnects.'; }
}
$('set-save').onclick = () => saveSettings(false);
$('set-restart').onclick = () => saveSettings(true);
// Editing a setting by hand leaves the profile.
for (const id of ['set-ppg', 'set-leds', ...NUMS.map((k) => `set-${k}`)]) $(id).addEventListener('input', () => { $('set-profile').value = 'custom'; });
setInterval(() => {
  if (source !== 'ble' || $('settings').hidden) return;
  const reply = scope.ble_settings_reply();
  if (reply.startsWith('error')) $('set-info').textContent = reply.split(';').slice(1).join(', ');
  else if (reply.startsWith('ok;written') || reply.startsWith('ok;removed')) $('set-info').textContent += ` · SD card: ${reply.split(';')[1]}`;
  const json = scope.ble_settings();
  if (!json || json === lastSettings) return;
  lastSettings = json;
  const c = JSON.parse(json);
  // The serial-number default shows as an empty name field.
  const serial = (c.device_id ?? '').split('-').pop();
  $('set-name').value = c.name === serial ? '' : c.name;
  $('set-ppg').value = c.ppg_hz; $('set-leds').checked = c.leds === '1';
  shown = { ppg_hz: c.ppg_hz, leds: c.leds ?? '1', profile: c.profile ?? 'custom' };
  // The clock it starts with next, so a pending change shows.
  for (const k of NUMS) {
    const v = k === 'cpu_mhz' ? c.cpu_mhz_next ?? c[k] : c[k];
    $(`set-${k}`).value = v ?? '';
    $(`set-${k}`).disabled = v === undefined; // firmware without the setting
    shown[k] = v;
  }
  $('set-profile').value = shown.profile;
  const from = { default: 'defaults', flash: 'set over Bluetooth', card: 'the SD card’s CONFIG.TXT (it overrides these at boot)' }[c.source] ?? c.source;
  $('set-info').textContent = `${c.device_id || 'no device id'} · settings from ${from}` + (c.restart_needed === '1' ? ' · a new name or CPU clock applies after a restart' : '') + (reply.startsWith('ok') ? ' · saved' : '');
}, 300);
$('rec-btn').onclick = () => {
  if (source === 'bridge') { send({ cmd: sdRecording ? 'record_end' : 'record_begin' }); return; }
  if (scope.is_recording()) saveRecording(); else startRecording();
};
$('note-btn').onclick = () => {
  const t = $('note').value.trim(); if (!t) return;
  if (source === 'ble') bleSend('UN', [localStamp(), t.replaceAll(',', ';')]);
  else send({ cmd: 'user_note', text: t });
  $('note').value = '';
};
$('note').onkeydown = (e) => { if (e.key === 'Enter') $('note-btn').click(); };
$('win+').onclick = () => scope.increment_window();
$('win-').onclick = () => scope.decrement_window();
$('pause').onclick = () => { scope.toggle_pause(); $('pause').textContent = scope.is_paused() ? 'Resume' : 'Pause'; };
$('grouped').onchange = () => { for (const { div } of plots.values()) div.remove(); plots.clear(); };
$('mode').onchange = (e) => {
  const mode = e.target.value; if (!mode) return;
  if (mode === 'hibernate' && !confirm('Hibernate powers the EmotiBit down; wake it with its button. Continue?')) { e.target.value = ''; return; }
  if (source === 'ble') {
    const tag = { normal: 'MN', low: 'ML', 'max-low': 'MM', 'wireless-off': 'MO', hibernate: 'MH' }[mode];
    bleSend(tag, [localStamp()]);
    if (mode === 'wireless-off' || mode === 'hibernate') banner('The board is turning its radio off; press its button to bring it back.', 8000, 'info');
  } else send({ cmd: 'set_power_mode', mode });
};
addEventListener('keydown', (e) => {
  if (['INPUT', 'SELECT', 'TEXTAREA'].includes(e.target.tagName) || $('toolbar').hidden) return;
  if (e.key === 'ArrowRight') scope.increment_window();
  if (e.key === 'ArrowLeft') scope.decrement_window();
  if (e.key === ' ') { e.preventDefault(); $('pause').click(); }
});
addEventListener('beforeunload', (e) => { if (scope.is_recording()) { e.preventDefault(); e.returnValue = ''; } });

// ── Plots ────────────────────────────────────────────────────────────────────────
const plots = new Map(); // key (tag or group) -> { div, canvas, val, tags, unit }
function plotKey(tag) { return $('grouped').checked && GROUP_OF[tag] ? GROUP_OF[tag] : tag; }
function ensurePlot(key, tags) {
  let p = plots.get(key);
  if (p) {
    for (const t of tags) if (!p.tags.includes(t)) p.tags.push(t);
    return p;
  }
  const div = document.createElement('div'); div.className = 'plot';
  const canvas = document.createElement('canvas');
  const head = document.createElement('div'); head.className = 'head';
  const lbl = document.createElement('span'); lbl.className = 'lbl';
  const val = document.createElement('span'); val.className = 'val num';
  const group = GROUPS[key];
  lbl.textContent = group ? GROUP_NAME[key] : scope.channel_name(key);
  lbl.style.color = group ? 'inherit' : color(key);
  head.append(lbl, val); div.append(canvas, head);
  p = { key, div, canvas, val, tags: [...tags], unit: scope.channel_unit(tags[0]) };
  plots.set(key, p);
  // Keep the grid in display order.
  const after = [...plots.values()].filter((q) => q !== p).sort((a, b) => rank(a.key) - rank(b.key)).find((q) => rank(q.key) > rank(key));
  $('grid').insertBefore(div, after?.div ?? null);
  return p;
}
const fmt = (v) => !Number.isFinite(v) ? '–' : Math.abs(v) >= 1000 ? v.toFixed(0) : Math.abs(v) >= 10 ? v.toFixed(1) : v.toFixed(3);
/// Tick labels with just enough decimals to tell the three ticks apart.
function tickFmt(lo, hi) {
  const span = Math.abs(hi - lo) / 2 || 1;
  const d = Math.min(6, Math.max(0, Math.ceil(-Math.log10(span)) + 1));
  return (v) => { const t = v.toFixed(d); return /^-0(\.0+)?$/.test(t) ? t.slice(1) : t; };
}
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' })[c]);

function draw(p) {
  const { canvas, tags } = p;
  const dpr = devicePixelRatio || 1;
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (!w || !h) return;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
  }
  const g = canvas.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0); g.clearRect(0, 0, w, h);
  const css = getComputedStyle(document.documentElement);
  let lo = Infinity, hi = -Infinity;
  for (const t of tags) { const [a, b] = scope.y_range(t); lo = Math.min(lo, a); hi = Math.max(hi, b); }
  // A flat signal (or none yet): centre it in a ±1 band rather than magnifying rounding noise.
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) { lo = -1; hi = 1; }
  else if (hi - lo < 1e-9 * Math.max(1, Math.abs(hi))) { const m = (hi + lo) / 2; lo = m - 1; hi = m + 1; }
  const win = scope.window_s();
  const padL = 56, padB = 16, padT = 8;
  const tick = tickFmt(lo, hi);
  g.strokeStyle = css.getPropertyValue('--grid'); g.lineWidth = 1; g.fillStyle = css.getPropertyValue('--muted');
  g.font = '10px ui-monospace, monospace';
  for (let i = 0; i <= 2; i++) {
    const y = padT + (h - padT - padB) * (i / 2);
    g.beginPath(); g.moveTo(padL, y); g.lineTo(w, y); g.stroke();
    g.fillText(tick(hi - (hi - lo) * (i / 2)), 6, y + 3);
  }
  g.fillText(`−${win} s`, padL, h - 4);
  g.fillText('now', w - 24, h - 4);
  const X = (x) => padL + (w - padL) * (1 + x / win);
  const Y = (v) => padT + (h - padT - padB) * (1 - (v - lo) / (hi - lo));
  for (const t of tags) {
    const pts = scope.window(t);
    g.strokeStyle = color(t); g.lineWidth = 1.5; g.beginPath();
    for (let i = 0; i < pts.length; i += 2) { const x = X(pts[i]), y = Y(pts[i + 1]); i === 0 ? g.moveTo(x, y) : g.lineTo(x, y); }
    g.stroke();
  }
  const unit = p.unit ? ` ${esc(p.unit)}` : '';
  if (tags.length > 1) {
    const names = p.key === 'SCR' ? { SA: 'amp', SR: 'rise s', SF: '/min' } : {};
    p.val.innerHTML = tags.map((t) => `<span style="color:${color(t)}">${names[t] ?? t.slice(-1).toLowerCase()}</span> <b>${fmt(scope.last_value(t))}</b>`).join('  ') + (p.key === 'SCR' ? '' : unit);
  } else {
    p.val.innerHTML = `<b>${fmt(scope.last_value(tags[0]))}</b>${unit}`;
  }
}
function frame() {
  if (source !== 'none') {
    const byKey = new Map();
    for (const tag of scope.tags()) {
      if (HIDDEN.has(tag)) continue;
      const k = plotKey(tag);
      byKey.set(k, [...(byKey.get(k) ?? []), tag]);
    }
    for (const [k, ts] of byKey) draw(ensurePlot(k, ts));
    $('empty').hidden = plots.size > 0 || $('toolbar').hidden;
  }
  $('win').textContent = `${scope.window_s()} s`;
  requestAnimationFrame(frame);
}

// ── Status ───────────────────────────────────────────────────────────────────────
const rate = { last: null };
setInterval(() => {
  const s = JSON.parse(scope.status_json());
  $('battery').textContent = s.battery_percent != null ? `${s.battery_percent.toFixed(0)}%` : '–';

  // Rates over about a second: packets (records for Bluetooth), and bytes on air for Bluetooth.
  const now = performance.now(), n = scope.packets(), link = scope.ble_link();
  const bytes = link.length === 4 ? link[3] : null;
  if (!rate.last) rate.last = { t: now, n, bytes };
  else if (now - rate.last.t >= 1000) {
    const dt = (now - rate.last.t) / 1000;
    rate.value = (n - rate.last.n) / dt;
    rate.last = { t: now, n, bytes };
  }
  // The firmware's byte counter only moves once a second, so average it over about 5 s.
  if (bytes != null) {
    rate.hist = (rate.hist ?? []).filter((h) => now - h.t <= 5000 && h.b <= bytes);
    rate.hist.push({ t: now, b: bytes });
    const h0 = rate.hist[0];
    rate.bps = now - h0.t >= 2000 ? (bytes - h0.b) / ((now - h0.t) / 1000) : null;
  }
  $('rate').textContent = rate.value != null && source !== 'none' ? `${rate.value.toFixed(0)}/s` : '–';

  // Loss: Bluetooth records are numbered per channel, so use the BLE session's own counter.
  const lost = source === 'ble' ? ble.lostBefore + scope.ble_counts()[1] : JSON.parse(scope.health_json()).lost;
  $('lost').textContent = lost; $('lost-chip').classList.toggle('bad', lost > 0);

  if (source === 'ble') {
    const fmtName = ble.format ?? '';
    if (link.length === 4) {
      // The firmware's byte counter covers what was handed to the radio before this record.
      const [dropped, mtu] = link;
      const bps = rate.bps != null ? ` · ${(rate.bps / 1000).toFixed(2)} kB/s` : '';
      $('link').textContent = `${fmtName} · MTU ${mtu}${bps}${dropped ? ` · ${dropped} dropped` : ''}`;
      $('link-chip').classList.toggle('bad', dropped > 0);
    } else $('link').textContent = fmtName;
  }

  sdRecording = !!s.recording;
  const browserRec = scope.is_recording();
  const recOn = source === 'bridge' ? sdRecording : browserRec;
  const sdOn = sdRecording && ['bridge', 'ble'].includes(source);
  $('rec-chip').hidden = !recOn && !sdOn;
  const parts = [];
  if (sdOn) parts.push(`SD ${s.recording_file ?? ''}`.trim());
  if (browserRec) parts.push(`REC ${recStarted ? Math.floor((Date.now() - recStarted) / 1000) : 0} s · ${scope.recorded_lines()} packets`);
  $('rec-text').textContent = parts.join(' · ');
  $('sd-btn').textContent = sdRecording ? '■ Stop SD recording' : '● Record on SD';
  $('sd-btn').classList.toggle('recording', sdRecording);
  if (source === 'ble' && s.power_mode) $('mode').value = { Normal: 'normal', Low: 'low', MaxLow: 'max-low', WirelessOff: 'wireless-off', Hibernate: 'hibernate' }[s.power_mode] ?? '';
  const btn = $('rec-btn');
  btn.textContent = recOn ? (source === 'bridge' ? '■ Stop SD recording' : '■ Stop & save') : (source === 'bridge' ? '● Record on SD' : '● Record');
  btn.classList.toggle('recording', recOn);
  btn.disabled = source === 'bridge' && ws?.readyState !== 1;
  $('clip').textContent = `${s.clipping_events}/${s.overflow_events}`;
  $('clip-chip').classList.toggle('bad', s.clipping_events + s.overflow_events > 0);

  const age = (performance.now() - lastPacketAt) / 1000;
  const live = ['ble', 'bridge', 'serial'].includes(source);
  $('foot').textContent = `${scope.packets()} packets, ${scope.malformed()} malformed` +
    (live && lastPacketAt && age > 2 ? ` · no data for ${age.toFixed(0)} s` : '');
}, 250);

// ── Start ────────────────────────────────────────────────────────────────────────
// Served by a bridge (it answers /bridge.json)? Connect back to it. Otherwise this is a static
// copy: use ?bridge=ws://host:port, the last-used address, or the default, and wait for a click.
const param = new URLSearchParams(location.search).get('bridge');
const stored = localStorage.getItem('emotibit-bridge');
let servedByBridge = false;
try { servedByBridge = (await fetch('./bridge.json', { cache: 'no-store' })).ok; } catch { /* static host or file:// */ }
$('bridge').value = param ?? (servedByBridge ? `ws://${location.host}/` : stored ?? 'ws://127.0.0.1:8765/');
requestAnimationFrame(frame);
showScope(false);
const q = new URLSearchParams(location.search);
if (q.has('demo')) startDemo();
else if (q.get('replay')) replayFromUrl(q.get('replay'));
else if (param || servedByBridge) $('connect').click();
