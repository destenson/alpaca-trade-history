// Alpaca Trade Analyzer - static, client-side front end.
// Data flow: browser -> Alpaca REST (GET only) -> bundle JSON -> Python (Pyodide) -> report HTML.
// Credentials never leave this page except in request headers to *.alpaca.markets (enforced by CSP).

const PYODIDE_INDEX = 'https://cdn.jsdelivr.net/pyodide/v0.26.4/full/';
const STORE_KEY = 'alpaca-analyzer.credentials.v1';
const TRADING = { live: 'https://api.alpaca.markets', paper: 'https://paper-api.alpaca.markets' };
const DATA = 'https://data.alpaca.markets';
const OCC = /^([A-Z0-9.]{1,6}?)(\d{6})([CP])(\d{8})$/;

const $ = (id) => document.getElementById(id);

// ------------------------------------------------------------------ credential storage
const b64 = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf)));
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));

async function deriveKey(pass, salt) {
  const base = await crypto.subtle.importKey('raw', new TextEncoder().encode(pass), 'PBKDF2', false, ['deriveKey']);
  return crypto.subtle.deriveKey({ name: 'PBKDF2', salt, iterations: 310000, hash: 'SHA-256' }, base,
    { name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
}

async function saveCreds(creds, pass) {
  let rec;
  if (pass) {
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const key = await deriveKey(pass, salt);
    const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, key, new TextEncoder().encode(JSON.stringify(creds)));
    rec = { v: 1, enc: true, env: creds.env, salt: b64(salt), iv: b64(iv), ct: b64(ct) };
  } else {
    rec = { v: 1, enc: false, ...creds };
  }
  try { localStorage.setItem(STORE_KEY, JSON.stringify(rec)); return true; } catch (e) { return false; }
}

function loadRecord() {
  try { return JSON.parse(localStorage.getItem(STORE_KEY) || 'null'); } catch (e) { return null; }
}

async function decryptRecord(rec, pass) {
  const key = await deriveKey(pass, unb64(rec.salt));
  const pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: unb64(rec.iv) }, key, unb64(rec.ct));
  return JSON.parse(new TextDecoder().decode(pt));
}

async function forget() {
  try { localStorage.removeItem(STORE_KEY); } catch (e) { /* storage unavailable */ }
  await lockDevice();
  location.reload();
}

// ------------------------------------------------- "keep unlocked on this device"
// After a passphrase unlock, the credentials are also encrypted under a random AES key that the browser
// generates as NON-EXTRACTABLE and keeps in IndexedDB. Scripts on this origin can ask the browser to
// decrypt with it, but nothing can read the key's bytes, so the stored data is useless if copied out of
// the browser profile. The unlock expires after 30 days without a visit, and Lock / Forget delete it.
const IDB_NAME = 'alpaca-analyzer';
const IDB_STORE = 'device';
const UNLOCK_ID = 'unlock';
const UNLOCK_DAYS = 30;

function idb() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(IDB_NAME, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(IDB_STORE);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}
async function idbDo(mode, fn) {
  const db = await idb();
  return new Promise((resolve, reject) => {
    const req = fn(db.transaction(IDB_STORE, mode).objectStore(IDB_STORE));
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function keepUnlocked(creds) {
  try {
    const key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, key, new TextEncoder().encode(JSON.stringify(creds)));
    await idbDo('readwrite', (st) => st.put({ key, iv, ct, expires: Date.now() + UNLOCK_DAYS * 864e5 }, UNLOCK_ID));
    return true;
  } catch (e) {
    console.warn('Could not keep the device unlocked:', e);
    return false;
  }
}

async function deviceUnlock() {
  try {
    const rec = await idbDo('readonly', (st) => st.get(UNLOCK_ID));
    if (!rec) return null;
    if (!(rec.expires > Date.now())) { await lockDevice(); return null; }
    const pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: rec.iv }, rec.key, rec.ct);
    rec.expires = Date.now() + UNLOCK_DAYS * 864e5;          // sliding window
    await idbDo('readwrite', (st) => st.put(rec, UNLOCK_ID));
    return JSON.parse(new TextDecoder().decode(pt));
  } catch (e) {
    await lockDevice();
    return null;
  }
}

async function lockDevice() {
  try { await idbDo('readwrite', (st) => st.delete(UNLOCK_ID)); } catch (e) { /* IndexedDB unavailable */ }
}

// ------------------------------------------------------------------------ progress UI
let currentStep = null;
let stepSink = null;      // during a refresh, progress goes to the inline status instead of the step list
let sinkText = '';
function step(text) {
  if (stepSink) { sinkText = text; stepSink(text); return; }
  if (currentStep) { currentStep.classList.remove('active'); currentStep.classList.add('done'); }
  currentStep = document.createElement('li');
  currentStep.textContent = text;
  currentStep.className = 'active';
  $('steps').appendChild(currentStep);
}
function note(text) {
  if (stepSink) { stepSink(`${sinkText} ${text}`); return; }
  if (currentStep) currentStep.textContent += ' ' + text;
}

// ----------------------------------------------------------------------- Alpaca client
class Alpaca {
  constructor({ key, secret, env }) {
    this.base = TRADING[env] || TRADING.live;
    this.headers = { 'APCA-API-KEY-ID': key, 'APCA-API-SECRET-KEY': secret, Accept: 'application/json' };
  }

  async get(url, params = {}) {
    const u = new URL(url);
    for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== '') u.searchParams.set(k, v);
    for (let attempt = 0; attempt < 5; attempt++) {
      let r;
      try {
        r = await fetch(u, { headers: this.headers, referrerPolicy: 'no-referrer', cache: 'no-store' });
      } catch (e) {
        throw new Error(`Network error calling ${u.host}${u.pathname}. Check your connection or ad-blocker.`);
      }
      if (r.status === 429 || r.status >= 500) { await new Promise((res) => setTimeout(res, 1000 * 2 ** attempt)); continue; }
      if (!r.ok) {
        const body = (await r.text()).slice(0, 300);
        const err = new Error(`${r.status} from ${u.host}${u.pathname}: ${body}`);
        err.status = r.status;
        throw err;
      }
      return r.json();
    }
    throw new Error(`Gave up on ${u.pathname} after repeated rate limits / server errors.`);
  }

  account() { return this.get(`${this.base}/v2/account`); }
  positions() { return this.get(`${this.base}/v2/positions`); }

  async activities() {
    const out = [];
    const params = { direction: 'asc', page_size: 100 };
    for (;;) {
      const page = await this.get(`${this.base}/v2/account/activities`, params);
      if (!page || !page.length) break;
      out.push(...page);
      note(`${out.length}…`);
      if (page.length < 100 || !page[page.length - 1].id) break;
      params.page_token = page[page.length - 1].id;
    }
    return out;
  }

  // Alpaca defaults to a 1-month period when `end` is omitted: always send it, in yearly windows.
  async portfolioHistory(startISO) {
    const merged = { timestamp: [], equity: [], cashflow: {}, timeframe: '1D' };
    const seen = new Set();
    let cur = new Date(startISO);
    const end = new Date();
    while (cur < end) {
      const nxt = new Date(Math.min(cur.getTime() + 365 * 864e5, end.getTime()));
      const r = await this.get(`${this.base}/v2/account/portfolio/history`, {
        timeframe: '1D', start: iso(cur), end: iso(nxt), cashflow_types: 'ALL', pnl_reset: 'no_reset' });
      (r.timestamp || []).forEach((t, i) => {
        if (seen.has(t)) return;
        seen.add(t);
        const n = merged.timestamp.push(t);
        merged.equity.push((r.equity || [])[i] ?? null);
        for (const [k, arr] of Object.entries(r.cashflow || {})) {
          const col = (merged.cashflow[k] ||= []);
          while (col.length < n - 1) col.push(0);
          col.push(arr[i] ?? 0);
        }
      });
      cur = nxt;
    }
    for (const col of Object.values(merged.cashflow)) while (col.length < merged.timestamp.length) col.push(0);
    return merged;
  }

  // Daily bars, split/dividend adjusted. Free plans can't query the last 15 min of SIP data, so end
  // 16 minutes ago; if SIP is refused altogether, fall back to the free IEX feed.
  async dailyBars(symbols, startISO) {
    const out = {};
    const end = iso(new Date(Date.now() - 16 * 60e3));
    let feed;
    const fetchChunk = async (chunk) => {
      const params = { symbols: chunk.join(','), timeframe: '1Day', start: startISO, end, adjustment: 'all',
        limit: 10000, feed };
      for (;;) {
        const r = await this.get(`${DATA}/v2/stocks/bars`, params);
        for (const [sym, bars] of Object.entries(r.bars || {})) {
          (out[sym] ||= []).push(...bars.map((b) => ({ t: b.t, c: b.c })));
        }
        if (!r.next_page_token) break;
        params.page_token = r.next_page_token;
      }
    };
    for (let i = 0; i < symbols.length; i += 50) {
      const chunk = symbols.slice(i, i + 50);
      try {
        await fetchChunk(chunk);
      } catch (e) {
        if (e.status === 403 && !feed) { feed = 'iex'; i -= 50; note('(using IEX feed)'); continue; }
        // One bad symbol fails the whole request: retry individually and skip the ones that fail.
        for (const s of chunk) { try { await fetchChunk([s]); } catch (_) { /* unknown or delisted */ } }
      }
    }
    return out;
  }

  async optionSnapshots(symbols) {
    const out = {};
    for (let i = 0; i < symbols.length; i += 100) {
      const syms = symbols.slice(i, i + 100).join(',');
      let r;
      try { r = await this.get(`${DATA}/v1beta1/options/snapshots`, { symbols: syms }); }
      catch (e) {
        try { r = await this.get(`${DATA}/v1beta1/options/snapshots`, { symbols: syms, feed: 'indicative' }); }
        catch (_) { continue; }
      }
      Object.assign(out, r.snapshots || {});
    }
    return out;
  }

  async latestPrices(symbols) {
    const out = {};
    for (let i = 0; i < symbols.length; i += 100) {
      let r;
      try { r = await this.get(`${DATA}/v2/stocks/snapshots`, { symbols: symbols.slice(i, i + 100).join(',') }); }
      catch (e) {
        try { r = await this.get(`${DATA}/v2/stocks/snapshots`, { symbols: symbols.slice(i, i + 100).join(','), feed: 'iex' }); }
        catch (_) { continue; }
      }
      for (const [s, snap] of Object.entries(r || {})) {
        const px = snap?.latestTrade?.p ?? snap?.dailyBar?.c;
        if (px) out[s] = px;
      }
    }
    return out;
  }
}

const iso = (d) => d.toISOString().replace(/\.\d{3}Z$/, 'Z');

function tradedUnderlyings(acts) {
  const s = new Set();
  for (const a of acts) {
    if (!a.symbol || !['FILL', 'OPTRD', 'OPEXP', 'OPASN', 'OPEXC'].includes(a.activity_type)) continue;
    const m = OCC.exec(a.symbol);
    s.add(m ? m[1] : a.symbol);
  }
  return [...s].filter((x) => /^[A-Z][A-Z.]{0,6}$/.test(x));
}

async function fetchBundle(creds, benchmarks) {
  const api = new Alpaca(creds);
  step('Checking keys and loading account…');
  let account;
  try { account = await api.account(); }
  catch (e) {
    if (e.status === 401 || e.status === 403) {
      throw new Error(`Alpaca rejected these keys for the ${creds.env} endpoint. Check the key pair and the Live/Paper switch.`);
    }
    throw e;
  }
  step('Downloading account activity…');
  const activities = await api.activities();
  const dates = activities.map((a) => (a.transaction_time || a.date || '').slice(0, 10)).filter(Boolean).sort();
  const first = dates[0] || new Date(Date.now() - 365 * 864e5).toISOString().slice(0, 10);
  step('Downloading daily equity history…');
  const portfolio_history = await api.portfolioHistory(`${first}T00:00:00Z`);
  note(`${portfolio_history.timestamp.length} days`);
  step('Downloading open positions…');
  const positions = await api.positions();
  const unds = tradedUnderlyings(activities);
  const syms = [...new Set([...unds, ...benchmarks])].sort();
  step(`Downloading daily prices for ${syms.length} symbols…`);
  const barStart = new Date(new Date(first).getTime() - 7 * 864e5).toISOString().slice(0, 10) + 'T00:00:00Z';
  const bars = await api.dailyBars(syms, barStart);
  const openOpts = positions.filter((p) => p.asset_class === 'us_option').map((p) => p.symbol);
  let option_snapshots = {};
  if (openOpts.length) {
    step(`Loading Greeks for ${openOpts.length} open option contracts…`);
    option_snapshots = await api.optionSnapshots(openOpts);
  }
  const missing = unds.filter((s) => !bars[s]);
  const prices = missing.length ? await api.latestPrices(missing) : {};
  return { account, activities, portfolio_history, positions, bars, option_snapshots, prices };
}

// --------------------------------------------------------------------------- Python
let pyReady = null;
function loadPython() {
  if (pyReady) return pyReady;
  pyReady = (async () => {
    const py = await loadPyodide({ indexURL: PYODIDE_INDEX });
    await py.loadPackage(['numpy', 'pandas', 'micropip', 'tzdata']);
    const manifest = await (await fetch('manifest.json', { cache: 'no-store' })).json();
    py.globals.set('wheel_url', new URL(manifest.wheel, location.href).href);
    await py.runPythonAsync('import micropip\nawait micropip.install(wheel_url, deps=False)');
    py.runPython('from alpaca_analyzer.web import build_reports');
    return py;
  })();
  return pyReady;
}

// ------------------------------------------------------------------------------- run
let reports = null;

let lastCreds = null;     // kept in memory for this tab only, so Refresh needs no re-entry
let currentView = 'full';
let currentTab = null;    // report tab the viewer is on (reported by the frame)
let refreshing = false;

async function analyze(creds) {
  const benchmarks = ($('bench').value || 'SPY QQQ').toUpperCase().split(/[\s,]+/).filter(Boolean);
  const pyLoading = loadPython();   // download Python in parallel with the API calls
  const bundle = await fetchBundle(creds, benchmarks);
  step('Starting the analysis engine (first visit downloads ~20 MB, then it\'s cached)…');
  const py = await pyLoading;
  step(`Rebuilding ${bundle.activities.length} activities into lots, strategies and charts…`);
  await new Promise((r) => setTimeout(r, 30));  // let the progress text paint
  const opts = { benchmarks, start: $('start').value || null };
  return JSON.parse(py.globals.get('build_reports')(JSON.stringify(bundle), JSON.stringify(opts)));
}

function applyResult(out) {
  reports = out;
  show(currentView);
  const s = out.summary;
  $('summary').innerHTML = s
    ? `<b>${fmtMoney(s.equity)}</b> equity · P&amp;L <b>${fmtMoney(s.pnl, true)}</b> · time-weighted <b>${fmtPct(s.twr)}</b> · max drawdown <b>${fmtPct(s.max_dd)}</b>`
    : 'Report ready.';
  $('updated').textContent = 'Updated ' + new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

async function run(creds) {
  lastCreds = creds;
  $('login').hidden = true;
  $('unlock').hidden = true;
  $('progress').hidden = false;
  $('error').hidden = true;
  $('steps').innerHTML = '';
  currentStep = null;
  try {
    const out = await analyze(creds);
    step('Done.');
    currentStep.classList.replace('active', 'done');
    applyResult(out);
    $('progress').hidden = true;
    $('result').hidden = false;
  } catch (e) {
    console.error(e);
    $('error').textContent = e.message || String(e);
    $('error').hidden = false;
    const back = document.createElement('button');
    back.textContent = 'Back';
    back.className = 'ghost';
    back.onclick = () => location.reload();
    $('error').after(back);
  }
}

// Re-fetch and re-analyze with the in-memory keys; the current report stays up until the new one is ready.
async function refresh() {
  if (!lastCreds || refreshing) return;
  refreshing = true;
  const btn = $('refresh');
  btn.disabled = true;
  btn.textContent = 'Refreshing…';
  $('refreshStatus').classList.remove('err');
  stepSink = (t) => { $('refreshStatus').textContent = '· ' + t; };
  try {
    applyResult(await analyze(lastCreds));
    $('refreshStatus').textContent = '';
  } catch (e) {
    console.error(e);
    $('refreshStatus').textContent = '· Refresh failed: ' + (e.message || String(e)) + ' Showing the previous report.';
    $('refreshStatus').classList.add('err');
  } finally {
    stepSink = null;
    refreshing = false;
    btn.disabled = false;
    btn.textContent = 'Refresh';
  }
}

function show(which) {
  currentView = which;
  $('frame').srcdoc = reports[which];
  $('viewFull').classList.toggle('on', which === 'full');
  $('viewShare').classList.toggle('on', which === 'share');
}

function download(which) {
  const blob = new Blob([reports[which]], { type: 'text/html' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = which === 'share' ? 'alpaca-report-shareable.html' : 'alpaca-report.html';
  a.click();
  setTimeout(() => URL.revokeObjectURL(a.href), 5000);
}

const fmtMoney = (v, signed) => (v < 0 ? '−' : signed && v > 0 ? '+' : '') + '$' +
  Math.abs(v).toLocaleString(undefined, { maximumFractionDigits: 0 });
const fmtPct = (v) => (v < 0 ? '−' : '+') + Math.abs(v * 100).toFixed(1) + '%';

// ------------------------------------------------------------------------------- theme
// The page owns the theme and pushes it into the report frame; the report's own Theme button asks the
// page to switch. Messages carry only a theme name and are accepted only from our own frame.
const THEME_KEY = 'alpaca-analyzer.theme';
const darkMQ = matchMedia('(prefers-color-scheme: dark)');
const effectiveTheme = () => document.documentElement.dataset.theme || (darkMQ.matches ? 'dark' : 'light');

function syncFrameTheme() {
  const w = $('frame').contentWindow;
  if (w) w.postMessage({ type: 'aa-set-theme', theme: effectiveTheme() }, '*');
}

function setTheme(t) {
  document.documentElement.dataset.theme = t;
  try { localStorage.setItem(THEME_KEY, t); } catch (e) { /* storage unavailable */ }
  syncFrameTheme();
}

$('themeBtn').addEventListener('click', () => setTheme(effectiveTheme() === 'dark' ? 'light' : 'dark'));
darkMQ.addEventListener('change', syncFrameTheme);
$('frame').addEventListener('load', syncFrameTheme);
window.addEventListener('message', (e) => {
  if (e.source !== $('frame').contentWindow || !e.data || typeof e.data !== 'object') return;
  if (e.data.type === 'aa-ready') {
    syncFrameTheme();
    if (currentTab) $('frame').contentWindow.postMessage({ type: 'aa-set-tab', tab: currentTab }, '*');
  }
  if (e.data.type === 'aa-tab' && typeof e.data.tab === 'string' && /^[a-z]{1,30}$/.test(e.data.tab)) currentTab = e.data.tab;
  if (e.data.type === 'aa-theme' && (e.data.theme === 'light' || e.data.theme === 'dark')) setTheme(e.data.theme);
});

// ------------------------------------------------------------------------------ wiring
$('origin').textContent = location.origin;
$('remember').addEventListener('change', (e) => {
  $('passrow').hidden = !e.target.checked;
  $('keeprow').hidden = !e.target.checked || !$('pass').value;
});
$('pass').addEventListener('input', () => { $('keeprow').hidden = !$('remember').checked || !$('pass').value; });

$('form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const creds = { key: $('key').value.trim(), secret: $('secret').value.trim(),
    env: document.querySelector('input[name=env]:checked').value };
  const pass = $('pass').value;
  await lockDevice();                       // new keys replace any previous unlock
  if ($('remember').checked) {
    const ok = await saveCreds(creds, pass);
    if (!ok) alert('This browser blocked local storage, so the keys were not saved.');
    if (ok && pass && $('keep').checked && await keepUnlocked(creds)) $('lock').hidden = false;
  } else {
    try { localStorage.removeItem(STORE_KEY); } catch (_) { /* storage unavailable */ }
  }
  $('pass').value = '';
  run(creds);
});

$('unlockForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  try {
    const creds = await decryptRecord(loadRecord(), $('unlockPass').value);
    $('unlockPass').value = '';
    if ($('unlockKeep').checked && await keepUnlocked(creds)) $('lock').hidden = false;
    run(creds);
  } catch (err) {
    $('unlockErr').textContent = 'Wrong passphrase.';
    $('unlockErr').hidden = false;
  }
});

$('forget').addEventListener('click', forget);
$('unlockForget').addEventListener('click', forget);
$('viewFull').addEventListener('click', () => show('full'));
$('viewShare').addEventListener('click', () => show('share'));
$('dlFull').addEventListener('click', () => download('full'));
$('dlShare').addEventListener('click', () => download('share'));
$('again').addEventListener('click', () => location.reload());
$('refresh').addEventListener('click', refresh);
$('lock').addEventListener('click', async () => {
  await lockDevice();
  $('lock').hidden = true;
  $('refreshStatus').classList.remove('err');
  $('refreshStatus').textContent = '· Locked: the passphrase will be needed next time.';
});

// Saved keys: plain ones prefill the form; encrypted ones open straight into the report while the
// device is kept unlocked, and otherwise ask for the passphrase.
const rec = loadRecord();
if (rec && rec.enc) {
  $('login').hidden = true;
  deviceUnlock().then((creds) => {
    if (creds) {
      $('lock').hidden = false;
      run(creds);
    } else {
      $('unlock').hidden = false;
      $('unlockPass').focus();
    }
  });
} else if (rec) {
  $('key').value = rec.key || '';
  $('secret').value = rec.secret || '';
  const env = document.querySelector(`input[name=env][value=${rec.env === 'paper' ? 'paper' : 'live'}]`);
  if (env) env.checked = true;
  $('remember').checked = true;
  $('forget').hidden = false;
  $('savedNote').textContent = 'Using keys saved on this device (unencrypted).';
}
// Warm the Python runtime while the user fills in the form.
if ('requestIdleCallback' in window) requestIdleCallback(() => loadPython().catch(() => { pyReady = null; }));
