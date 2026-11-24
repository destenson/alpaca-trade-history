// Alpaca Trade Analyzer - static, client-side front end.
// Data flow: browser -> Alpaca REST (GET only) -> bundle JSON -> analysis engine (Rust, WebAssembly) -> report HTML.
// Credentials never leave this page except in request headers to *.alpaca.markets (enforced by CSP).

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
  await clearCache();
}

// ---------------------------------------------------------------------- data cache
// Account history (activities, daily bars, equity history) doesn't change once it's in the past, so it's
// kept in IndexedDB and each refresh downloads only what's new. It's account data, so it's stored
// AES-GCM-encrypted under its own random NON-EXTRACTABLE key (same protection as "keep unlocked"), one
// record per key pair. Lock and "Forget saved keys" delete it.
const CACHE_KEY_ID = 'cachekey';
const CACHE_VERSION = 1;
const BARS_FULL_DAYS = 7;       // re-download all bars weekly (adjusted history changes on dividends)

async function cacheId(creds) {
  const h = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(`${creds.env}:${creds.key}`));
  return 'cache:' + b64(h).slice(0, 22);
}
async function cacheKey(create) {
  let key = await idbDo('readonly', (st) => st.get(CACHE_KEY_ID));
  if (!key && create) {
    key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    await idbDo('readwrite', (st) => st.put(key, CACHE_KEY_ID));
  }
  return key;
}
async function loadCache(creds) {
  try {
    const id = await cacheId(creds);
    const rec = await idbDo('readonly', (st) => st.get(id));
    const key = rec && await cacheKey(false);
    if (!rec || !key) return null;
    const pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: rec.iv }, key, rec.ct);
    const c = JSON.parse(new TextDecoder().decode(pt));
    return c.v === CACHE_VERSION ? c : null;
  } catch (e) {
    console.warn('Ignoring unreadable data cache:', e);
    return null;
  }
}
async function saveCache(creds, data) {
  try {
    const key = await cacheKey(true);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ct = await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, key,
      new TextEncoder().encode(JSON.stringify({ ...data, v: CACHE_VERSION })));
    const id = await cacheId(creds);
    await idbDo('readwrite', (st) => st.put({ iv, ct, savedAt: Date.now() }, id));
  } catch (e) {
    console.warn('Could not save the data cache:', e);
  }
}
async function clearCache() {
  try {
    const keys = await idbDo('readonly', (st) => st.getAllKeys());
    for (const k of keys) if (String(k).startsWith('cache')) await idbDo('readwrite', (st) => st.delete(k));
  } catch (e) { /* IndexedDB unavailable */ }
}

// Is anything trading right now? Regular 9:30-16:00 is covered by the clock; pre-market (4:00), after
// hours (to 20:00) and the overnight session (Sun-Thu 20:00 -> 4:00) can still move stock prices while
// option quotes are frozen. Friday 20:00 -> Sunday 20:00 ET nothing trades.
function stockSessionLive(now = new Date()) {
  const p = Object.fromEntries(new Intl.DateTimeFormat('en-US', { timeZone: 'America/New_York', weekday: 'short',
    hour: 'numeric', hour12: false }).formatToParts(now).map((x) => [x.type, x.value]));
  const h = +p.hour % 24, d = p.weekday;
  if (d === 'Sat') return false;
  if (d === 'Sun') return h >= 20;
  if (d === 'Fri') return h < 20;
  return true;
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
    this.calls = 0;         // requests made, shown in the refresh status
  }

  async get(url, params = {}) {
    const u = new URL(url);
    for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== '') u.searchParams.set(k, v);
    for (let attempt = 0; attempt < 5; attempt++) {
      let r;
      this.calls++;
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

  // Order changes. Never retried: a repeat could act twice. Errors carry Alpaca's message.
  async send(method, path, body) {
    this.calls++;
    let r;
    try {
      r = await fetch(`${this.base}${path}`, { method, referrerPolicy: 'no-referrer', cache: 'no-store',
        headers: body ? { ...this.headers, 'Content-Type': 'application/json' } : this.headers, body: body ? JSON.stringify(body) : undefined });
    } catch (e) {
      const err = new Error('network error: the request may or may not have reached Alpaca');
      err.network = true;
      throw err;
    }
    const text = await r.text();
    if (!r.ok) {
      let msg = text;
      try { msg = JSON.parse(text).message || text; } catch (_) { /* not JSON */ }
      const err = new Error(`${r.status}: ${msg.slice(0, 300)}`);
      err.status = r.status;
      throw err;
    }
    return text ? JSON.parse(text) : null;
  }
  replaceOrder(id, body) { return this.send('PATCH', `/v2/orders/${encodeURIComponent(id)}`, body); }
  cancelOrder(id) { return this.send('DELETE', `/v2/orders/${encodeURIComponent(id)}`); }
  getOrder(id) { return this.send('GET', `/v2/orders/${encodeURIComponent(id)}?nested=true`); }
  placeOrder(body) { return this.send('POST', '/v2/orders', body); }
  closedOrders(after) {
    return this.get(`${this.base}/v2/orders`, { status: 'closed', nested: true, after, limit: 100, direction: 'desc' });
  }
  positions() { return this.get(`${this.base}/v2/positions`); }
  clock() { return this.get(`${this.base}/v2/clock`); }
  openOrders() { return this.get(`${this.base}/v2/orders?status=open&nested=true&limit=500&direction=desc`); }

  async activities(after) {
    const out = [];
    const params = { direction: 'asc', page_size: 100, after };
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

  // 5-minute closes over the last two weeks (regular and extended hours; the engine keeps the regular session)
  async intradayBars(symbols) {
    const out = {};
    const params = { symbols: symbols.join(','), timeframe: '5Min', start: iso(new Date(Date.now() - 14 * 864e5)),
      end: iso(new Date(Date.now() - 16 * 60e3)), limit: 10000 };
    for (const feed of [undefined, 'iex']) {
      try {
        const p = { ...params, feed };
        for (;;) {
          const r = await this.get(`${DATA}/v2/stocks/bars`, p);
          for (const [sym, bars] of Object.entries(r.bars || {})) (out[sym] ||= []).push(...bars.map((b) => ({ t: b.t, c: b.c })));
          if (!r.next_page_token) break;
          p.page_token = r.next_page_token;
        }
        return out;
      } catch (e) {
        if (e.status !== 403 || feed) return out;
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

  // Raw stock snapshots (latest trade with time/size/conditions + daily bar). `feed` 'overnight' needs a
  // market-data subscription; without one it just comes back empty.
  async stockSnapshots(symbols, feed) {
    const out = {};
    for (let i = 0; i < symbols.length; i += 100) {
      const syms = symbols.slice(i, i + 100).join(',');
      let r;
      try { r = await this.get(`${DATA}/v2/stocks/snapshots`, { symbols: syms, feed }); }
      catch (e) {
        if (feed) continue;
        try { r = await this.get(`${DATA}/v2/stocks/snapshots`, { symbols: syms, feed: 'iex' }); }
        catch (_) { continue; }
      }
      for (const [k, v] of Object.entries(r || {})) if (v) out[k] = v;
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
        // extended-hours prints (condition T/U) can sit far from the session close on thin books
        const ext = (snap?.latestTrade?.c || []).some((c) => c === 'T' || c === 'U');
        const px = (ext ? snap?.dailyBar?.c : null) ?? snap?.latestTrade?.p ?? snap?.dailyBar?.c;
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

// Underlyings of open option orders where no contract has a quote, a trade or an implied volatility yet
// (a newly listed chain): the engine prices those legs from the stock's own recent volatility instead.
function unquotedUnderlyings(orders, snaps) {
  const quoted = new Set(), all = new Set();
  const has = (s) => s && ((s.latestQuote && (s.latestQuote.bp > 0 || s.latestQuote.ap > 0)) || s.latestTrade?.p > 0 || s.impliedVolatility > 0);
  for (const [sym, s] of Object.entries(snaps || {})) if (has(s)) quoted.add(OCC.exec(sym)?.[1]);
  for (const sym of orderOptionSymbols(orders)) all.add(OCC.exec(sym)[1]);
  return [...all].filter((u) => !quoted.has(u)).sort();
}
async function addIntraday(api, b) {
  const want = unquotedUnderlyings(b.orders, b.option_snapshots);
  if (!want.length) { b.intraday = {}; return; }
  step(`No option quotes yet for ${want.join(', ')}: loading recent 5-minute prices to estimate volatility…`);
  try { b.intraday = await api.intradayBars(want); } catch (_) { b.intraday = {}; }
}

function orderOptionSymbols(orders) {
  const out = new Set();
  for (const o of orders) {
    for (const sym of [o.symbol, ...(o.legs || []).map((l) => l.symbol)]) if (sym && OCC.test(sym)) out.add(sym);
  }
  return out;
}
const actDate = (a) => (a.transaction_time || a.date || '').slice(0, 10);
const posKey = (ps) => ps.map((p) => `${p.symbol}:${p.qty}`).sort().join(',');

function mergeActivities(old, fresh) {
  const seen = new Set(old.map((a) => a.id));
  return [...old, ...fresh.filter((a) => !a.id || !seen.has(a.id))];
}
function mergeHistory(old, fresh) {
  if (!fresh.timestamp.length) return old;
  const cut = fresh.timestamp[0];
  const keep = old.timestamp.findIndex((t) => t >= cut);
  const n = keep < 0 ? old.timestamp.length : keep;
  const out = { timestamp: [...old.timestamp.slice(0, n), ...fresh.timestamp],
                equity: [...old.equity.slice(0, n), ...fresh.equity], cashflow: {}, timeframe: '1D' };
  for (const k of new Set([...Object.keys(old.cashflow || {}), ...Object.keys(fresh.cashflow || {})])) {
    const a = (old.cashflow[k] || []).slice(0, n);
    while (a.length < n) a.push(0);
    const b = fresh.cashflow[k] || fresh.timestamp.map(() => 0);
    out.cashflow[k] = [...a, ...b];
  }
  return out;
}
// Merge freshly downloaded bars (which overlap the cached ones by a few days). Returns the symbols whose
// overlapping closes disagree with the cached copy: their adjusted history was rewritten (a split or a
// dividend), so the cached older bars are on the wrong scale and the full history must be re-downloaded.
function mergeBars(old, fresh) {
  const out = { ...old };
  const rescaled = [];
  for (const [sym, bars] of Object.entries(fresh)) {
    if (!bars.length) continue;
    const prev = new Map((old[sym] || []).map((b) => [b.t, b.c]));
    if (bars.some((b) => prev.has(b.t) && Math.abs(b.c / prev.get(b.t) - 1) > 0.005)) rescaled.push(sym);
    const cut = bars[0].t;
    out[sym] = [...(old[sym] || []).filter((b) => b.t < cut), ...bars];
  }
  return { bars: out, rescaled };
}

async function fetchSnapshots(api, positions, orders, unds, bars, prev) {
  const orderOpts = orderOptionSymbols(orders);
  const orderUnds = [...orderOpts].map((s) => OCC.exec(s)[1]);
  const openOpts = [...new Set([...positions.filter((p) => p.asset_class === 'us_option').map((p) => p.symbol), ...orderOpts])];
  const rpUnds = [...new Set([...orderUnds, ...openOpts.map((s) => OCC.exec(s)[1])])].sort();
  let option_snapshots = {};
  if (openOpts.length) {
    step(`Loading quotes and Greeks for ${openOpts.length} option contracts (positions + open orders)…`);
    option_snapshots = await api.optionSnapshots(openOpts);
  }
  const missing = [...new Set([...unds.filter((s) => !bars[s]), ...orderUnds])];
  const prices = missing.length ? await api.latestPrices(missing) : {};
  let clock = null, stock_snapshots = {};
  if (rpUnds.length) {
    step('Checking after-hours and overnight prices for option underlyings…');
    try { clock = await api.clock(); } catch (_) { clock = prev?.clock || null; }
    stock_snapshots = { default: await api.stockSnapshots(rpUnds) };
    const night = await api.stockSnapshots(rpUnds, 'overnight');
    if (Object.keys(night).length) stock_snapshots.overnight = night;
  } else {
    try { clock = await api.clock(); } catch (_) { clock = null; }
  }
  return { option_snapshots, prices, clock, stock_snapshots };
}

// mode: 'full' (no cache / forced), 'update' (download only what's new), 'closed' (market closed since the
// cached copy: only open orders and positions are checked, plus stock prints while a session trades)
async function fetchBundle(creds, benchmarks, force = false) {
  const api = new Alpaca(creds);
  const cache = force ? null : await loadCache(creds);
  const now = Date.now();
  const clockC = cache?.clock;
  const closedSince = cache && clockC && !clockC.is_open && clockC.next_open && now < Date.parse(clockC.next_open);
  const syms = (acts) => [...new Set([...tradedUnderlyings(acts), ...benchmarks])].sort();

  let pre = null;              // positions/orders already fetched by the closed-market check
  if (closedSince) {
    step('Market closed since the last download: checking open orders and positions…');
    let positions, orders;
    try { [positions, orders] = [await api.positions(), await api.openOrders()]; }
    catch (e) {
      if (e.status === 401 || e.status === 403) throw new Error(`Alpaca rejected these keys for the ${creds.env} endpoint.`);
      throw e;
    }
    if (posKey(positions) === posKey(cache.positions || [])) {
      const b = { ...cache, positions, orders };
      // order legs or benchmarks not seen before: fetch just those
      const newOpts = [...orderOptionSymbols(orders)].filter((s) => !b.option_snapshots[s]);
      b.option_snapshots = { ...(b.option_snapshots || {}) };
      if (newOpts.length) Object.assign(b.option_snapshots, await api.optionSnapshots(newOpts));
      const rpUnds = [...new Set([...orderOptionSymbols(orders)].map((s) => OCC.exec(s)[1]))];
      const newUnds = rpUnds.filter((u) => !(b.stock_snapshots?.default || {})[u]);
      if (stockSessionLive() || newUnds.length) {
        const want = stockSessionLive() ? [...new Set([...rpUnds, ...Object.keys(b.stock_snapshots?.default || {})])] : newUnds;
        step('Checking after-hours / overnight prices…');
        const dflt = await api.stockSnapshots(want);
        const night = await api.stockSnapshots(want, 'overnight');
        b.stock_snapshots = { default: { ...(b.stock_snapshots?.default || {}), ...dflt },
                              overnight: { ...(b.stock_snapshots?.overnight || {}), ...night } };
      }
      const tried = new Set(b.barsTried || Object.keys(b.bars));
      const needBars = syms(b.activities).filter((s) => !tried.has(s));
      if (needBars.length) {
        b.bars = { ...b.bars, ...await api.dailyBars(needBars, b.barStart) };
        b.barsTried = [...tried, ...needBars];
      }
      await addIntraday(api, b);
      await saveCache(creds, { ...b, savedAt: cache.savedAt });
      return { bundle: b, meta: { mode: 'closed', calls: api.calls, savedAt: cache.savedAt } };
    }
    step('Positions changed since the last download (expiration, assignment or a fill): updating…');
    pre = { positions, orders };
  }

  step('Checking keys and loading account…');
  let account;
  try { account = await api.account(); }
  catch (e) {
    if (e.status === 401 || e.status === 403) {
      throw new Error(`Alpaca rejected these keys for the ${creds.env} endpoint. Check the key pair and the Live/Paper switch.`);
    }
    throw e;
  }
  let activities, portfolio_history, bars, barStart, barsFullAt, newActs = [];
  if (cache) {
    const last = cache.activities.map(actDate).filter(Boolean).sort().pop();
    const after = last ? new Date(Date.parse(last) - 3 * 864e5).toISOString().slice(0, 10) : undefined;
    step('Downloading new account activity…');
    const fresh = await api.activities(after);
    const known = new Set(cache.activities.map((a) => a.id));
    newActs = fresh.filter((a) => !a.id || !known.has(a.id));
    activities = mergeActivities(cache.activities, fresh);
    note(`${newActs.length} new`);
    const lastTs = cache.portfolio_history.timestamp.at(-1);
    step('Updating daily equity history…');
    portfolio_history = lastTs
      ? mergeHistory(cache.portfolio_history, await api.portfolioHistory(iso(new Date((lastTs - 7 * 86400) * 1000))))
      : cache.portfolio_history;
  } else {
    step('Downloading account activity…');
    activities = await api.activities();
    const first = activities.map(actDate).filter(Boolean).sort()[0] || new Date(now - 365 * 864e5).toISOString().slice(0, 10);
    step('Downloading daily equity history…');
    portfolio_history = await api.portfolioHistory(`${first}T00:00:00Z`);
    note(`${portfolio_history.timestamp.length} days`);
  }
  const first = activities.map(actDate).filter(Boolean).sort()[0] || new Date(now - 365 * 864e5).toISOString().slice(0, 10);
  barStart = new Date(new Date(first).getTime() - 7 * 864e5).toISOString().slice(0, 10) + 'T00:00:00Z';
  step('Downloading open positions…');
  const positions = pre ? pre.positions : await api.positions();
  const allSyms = syms(activities);
  let barsTried;
  const weekly = !cache || !(now - (cache.barsFullAt || 0) < BARS_FULL_DAYS * 864e5) || cache.barStart !== barStart;
  if (weekly) {
    step(`Downloading daily prices for ${allSyms.length} symbols…`);
    bars = await api.dailyBars(allSyms, barStart);
    barsFullAt = now;
    barsTried = allSyms;
  } else {
    // a split rewrites a symbol's whole adjusted history; new symbols need their full history too
    const split = new Set(newActs.filter((a) => /^(SPLIT|SPIN|REORG|NC)$/.test(a.activity_type || '') && a.symbol)
      .map((a) => (OCC.exec(a.symbol)?.[1] || a.symbol)));
    const tried = new Set(cache.barsTried || Object.keys(cache.bars));
    const full = allSyms.filter((s) => !tried.has(s) || split.has(s));
    const inc = allSyms.filter((s) => !full.includes(s) && cache.bars[s]);   // symbols with no bars stay skipped
    barsTried = [...new Set([...tried, ...allSyms])];
    step(`Updating daily prices (${inc.length} symbols${full.length ? `, ${full.length} in full` : ''})…`);
    const lastDay = inc.map((s) => cache.bars[s]?.at(-1)?.t).filter(Boolean).sort()[0];
    const incStart = lastDay ? new Date(Date.parse(lastDay) - 5 * 864e5).toISOString().slice(0, 10) + 'T00:00:00Z' : barStart;
    const merged = mergeBars(cache.bars, inc.length ? await api.dailyBars(inc, incStart) : {});
    bars = merged.bars;
    const redo = [...new Set([...full, ...merged.rescaled])];
    if (merged.rescaled.length) note(`(re-adjusted history: ${merged.rescaled.join(', ')})`);
    if (redo.length) bars = { ...bars, ...await api.dailyBars(redo, barStart) };
    barsFullAt = cache.barsFullAt;
  }
  step('Downloading open orders…');
  const orders = pre ? pre.orders : await api.openOrders();
  const snaps = await fetchSnapshots(api, positions, orders, tradedUnderlyings(activities), bars, cache);
  const bundle = { account, activities, portfolio_history, positions, bars, orders, ...snaps };
  await addIntraday(api, bundle);
  await saveCache(creds, { ...bundle, barStart, barsFullAt, barsTried, savedAt: now });
  return { bundle, meta: { mode: cache ? 'update' : 'full', calls: api.calls, newActs: newActs.length } };
}

// ------------------------------------------------------------------------------ engine
// The analysis engine is a Rust program compiled to WebAssembly (engine-<build>.wasm, ~0.5 MB). It has a
// tiny C-style interface: copy two UTF-8 strings in, call aa_run, read the JSON result back out.
let engineReady = null;
function loadEngine() {
  if (engineReady) return engineReady;
  engineReady = (async () => {
    let file = MY_BUILD ? `engine-${MY_BUILD}.wasm` : null;
    if (!file) file = (await (await fetch('manifest.json', { cache: 'no-store' })).json()).engine;   // local preview
    const res = await fetch(file);
    if (!res.ok) {
      throw new Error(res.status === 404 ? 'A new version of this page was published: reload the page to use it.'
        : `Could not download the analysis engine (HTTP ${res.status}).`);
    }
    return WebAssembly.compile(await res.arrayBuffer());
  })();
  return engineReady;
}

async function runEngine(mod, bundleJson, optsJson) {
  const { exports: x } = await WebAssembly.instantiate(mod, {});   // a fresh instance (and heap) per run
  const enc = new TextEncoder();
  const put = (str) => {
    const b = enc.encode(str);
    const p = x.aa_alloc(b.length);
    new Uint8Array(x.memory.buffer, p, b.length).set(b);
    return [p, b.length];
  };
  const read = (p, n) => new TextDecoder().decode(new Uint8Array(x.memory.buffer, p, n));
  const [bp, bl] = put(bundleJson);
  const [op, ol] = put(optsJson);
  try {
    x.aa_run(bp, bl, op, ol, Date.now(), -new Date().getTimezoneOffset());
  } catch (e) {   // a panic in the engine aborts the call; it left its message behind
    const msg = read(x.aa_panic_ptr(), x.aa_panic_len());
    throw new Error('The analysis engine failed' + (msg ? `: ${msg}` : ` (${e.message})`));
  }
  const out = JSON.parse(read(x.aa_out_ptr(), x.aa_out_len()));
  if (out.error) throw new Error(out.error);
  return out;
}

// ------------------------------------------------------------------------------- run
let reports = null;

let lastCreds = null;     // kept in memory for this tab only, so Refresh needs no re-entry
let currentView = 'full';
let currentTab = null;    // report tab the viewer is on (reported by the frame)
try {                     // restored after an automatic reload for a new version
  const keep = JSON.parse(sessionStorage.getItem('aa-resume') || 'null');
  sessionStorage.removeItem('aa-resume');
  if (keep) { currentTab = /^[a-z]{1,30}$/.test(keep.tab || '') ? keep.tab : null; currentView = keep.view === 'share' ? 'share' : 'full'; }
} catch (e) { /* storage unavailable */ }

// ------------------------------------------------------------------ new deployments
// Every deployment has a new build id (in app.js?v=<build> and manifest.json). When a newer one is live,
// reload into it: straight back to the report if this device is kept unlocked, otherwise offer a button
// (reloading would drop keys held only in this tab's memory).
const MY_BUILD = new URL(import.meta.url).searchParams.get('v');
async function newerBuild() {
  if (!MY_BUILD) return false;          // local preview without build stamps
  try {
    const m = await (await fetch('manifest.json', { cache: 'no-store' })).json();
    return Boolean(m.build) && m.build !== MY_BUILD;
  } catch (e) { return false; }
}
function reloadForUpdate() {
  try { sessionStorage.setItem('aa-resume', JSON.stringify({ tab: currentTab, view: currentView })); } catch (e) { /* ignore */ }
  location.reload();
}
async function checkForUpdate() {
  if (refreshing || !(await newerBuild())) return false;
  const st = $('refreshStatus');
  if (st.querySelector('.linkbtn')) return true;       // already offered
  st.classList.remove('err');
  if (!$('lock').hidden) {              // kept unlocked: the reload reopens the report by itself
    st.textContent = '· A new version is live: reloading…';
    setTimeout(reloadForUpdate, 600);
  } else {
    st.textContent = '· A new version is live. ';
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'linkbtn'; b.textContent = 'Reload to update';
    b.title = 'Reload the page; keys that were only held in this tab will need to be entered again';
    b.onclick = reloadForUpdate;
    st.append(b);
  }
  return true;
}
// backup for a tab left open: look for a new version every few minutes (reloads by itself when it can)
setInterval(() => { if (!$('result').hidden) checkForUpdate(); }, 5 * 60e3);
document.addEventListener('visibilitychange', async () => {
  if (document.visibilityState === 'visible' && !$('result').hidden && !(await checkForUpdate())) autoRefresh();
});
// switching back from another window (not another tab) only fires focus
let lastFocusCheck = 0;
window.addEventListener('focus', () => {
  if ($('result').hidden || Date.now() - lastFocusCheck < 30e3) return;
  lastFocusCheck = Date.now();
  checkForUpdate().then((upd) => { if (!upd) autoRefresh(); });
});
let refreshing = false;

// Keep the report current without a manual Refresh: on switching report tabs or coming back to the
// page, re-check orders, positions and quotes (at most every 30 s) and redraw only if something changed.
// Skipped while the order popup is open or the re-pricing inputs hold edits (a redraw would drop them).
let lastAutoAt = 0, rpEdited = false;
// Update once more a minute after the next market open (fresh quotes), whatever the page is doing.
let openTimer = null;
function scheduleOpen(clock) {
  clearTimeout(openTimer);
  const t = clock && !clock.is_open && Date.parse(clock.next_open || '');
  if (!t) return;
  const wait = Math.min(Math.max(t - Date.now(), 0) + 60e3, 2 ** 31 - 1);
  openTimer = setTimeout(function tick() {
    if (!lastCreds) return;
    if (refreshing || $('ordDlg').open || rpEdited) { openTimer = setTimeout(tick, 60e3); return; }   // try again shortly
    lastAutoAt = 0;
    refresh(false, true);
  }, wait);
}
function autoRefresh() {
  if (!lastCreds || refreshing || $('result').hidden || $('ordDlg').open || rpEdited) return;
  if (Date.now() - lastAutoAt < 30e3) return;
  refresh(false, true);
}

let lastMeta = null;
let lastOrders = [];
let lastPositions = [];
let lastCancelled = [];    // option limit orders cancelled in the last day and not placed again (Re-place)       // open orders as Alpaca returned them, for the change-price popup
// What a report is built from that changes during the day: open orders, positions and their marks,
// quotes and stock prints. Equal signatures mean re-running the engine would draw the same report.
function bundleSig(b) {
  const q = (s) => [s?.latestQuote?.bp, s?.latestQuote?.ap, s?.latestTrade?.p, s?.impliedVolatility];
  return JSON.stringify([
    (b.orders || []).map((o) => [o.id, o.status, o.limit_price, o.stop_price, o.qty, o.filled_qty]),
    (b.positions || []).map((p) => [p.symbol, p.qty, p.current_price]),
    Object.entries(b.option_snapshots || {}).map(([k, v]) => [k, q(v)]),
    Object.entries(b.stock_snapshots || {}).map(([f, book]) => [f, Object.entries(book || {}).map(([k, v]) => [k, v?.latestTrade?.p, v?.dailyBar?.c])]),
    b.prices, (b.activities || []).length, b.clock?.is_open,
  ]);
}
let lastSig = null;
async function analyze(creds, force = false, onlyIfChanged = false) {
  const benchmarks = ($('bench').value || 'SPY QQQ').toUpperCase().split(/[\s,]+/).filter(Boolean);
  const engine = loadEngine();   // download the engine in parallel with the API calls
  const { bundle, meta } = await fetchBundle(creds, benchmarks, force);
  lastMeta = meta;
  lastOrders = bundle.orders || [];
  lastPositions = bundle.positions || [];
  lastCancelled = await recentCancels(new Alpaca(creds), lastOrders);
  scheduleOpen(bundle.clock);
  const sig = bundleSig(bundle);
  if (onlyIfChanged && sig === lastSig) return null;      // nothing new: keep the report as it is
  lastSig = sig;
  step('Starting the analysis engine…');
  const mod = await engine;
  step(`Rebuilding ${bundle.activities.length} activities into lots, strategies and charts…`);
  await new Promise((r) => setTimeout(r, 30));  // let the progress text paint
  const opts = { benchmarks, start: $('start').value || null };
  return runEngine(mod, JSON.stringify(bundle), JSON.stringify(opts));
}

function applyResult(out) {
  reports = out;
  renderCancelled();
  show(currentView);
  const s = out.summary;
  $('summary').innerHTML = s
    ? `<b>${fmtMoney(s.equity)}</b> equity · P&amp;L <b>${fmtMoney(s.pnl, true)}</b> · time-weighted <b>${fmtPct(s.twr)}</b> · max drawdown <b>${fmtPct(s.max_dd)}</b>`
    : 'Report ready.';
  const t = (ms) => new Date(ms).toLocaleString([], { weekday: 'short', hour: 'numeric', minute: '2-digit' });
  const m = lastMeta || {};
  const how = m.mode === 'closed'
    ? ` · market closed: ${m.calls} request${m.calls === 1 ? '' : 's'}, history from the copy saved ${t(m.savedAt)}`
    : m.mode === 'update' ? ` · ${m.calls} requests, only new data (${m.newActs} new activit${m.newActs === 1 ? 'y' : 'ies'})`
    : m.mode === 'full' ? ` · full download, ${m.calls} requests` : '';
  $('updated').textContent = 'Updated ' + new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) + how;
  $('refreshAll').hidden = !(m.mode === 'closed' || m.mode === 'update');
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
    lastAutoAt = Date.now();
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
async function refresh(force = false, auto = false) {
  if (!lastCreds || refreshing) return;
  lastAutoAt = Date.now();
  const btn = $('refresh');
  btn.disabled = true;                  // immediate feedback, also while checking for a new version
  btn.textContent = 'Refreshing…';
  if (await checkForUpdate()) { btn.disabled = false; btn.textContent = 'Refresh'; return; }   // the new version re-runs the report itself
  refreshing = true;
  $('refreshStatus').classList.remove('err');
  stepSink = (t) => { $('refreshStatus').textContent = '· ' + t; };
  try {
    const out = await analyze(lastCreds, force, auto);
    if (out) applyResult(out);
    $('refreshStatus').textContent = auto && out ? '· Updated: orders or quotes changed' : '';
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
// charts the viewer switched to log scale (ids only), restored into every report
const LOG_STORE = 'alpaca-analyzer.logcharts';
function logCharts() {
  try { return (JSON.parse(localStorage.getItem(LOG_STORE) || '[]') || []).filter((x) => typeof x === 'string'); }
  catch (_) { return []; }
}
window.addEventListener('message', (e) => {
  if (e.source !== $('frame').contentWindow || !e.data || typeof e.data !== 'object') return;
  if (e.data.type === 'aa-ready') {
    syncFrameTheme();
    if (currentTab) $('frame').contentWindow.postMessage({ type: 'aa-set-tab', tab: currentTab }, '*');
    const ids = logCharts();
    if (ids.length) $('frame').contentWindow.postMessage({ type: 'aa-set-log', ids }, '*');
  }
  if (e.data.type === 'aa-log' && typeof e.data.id === 'string' && /^f_[a-z0-9_]{1,40}$/.test(e.data.id)) {
    const ids = new Set(logCharts());
    if (e.data.on) ids.add(e.data.id); else ids.delete(e.data.id);
    try { localStorage.setItem(LOG_STORE, JSON.stringify([...ids])); } catch (_) { /* storage unavailable */ }
  }
  if (e.data.type === 'aa-tab' && typeof e.data.tab === 'string' && /^[a-z]{1,30}$/.test(e.data.tab)) {
    const switched = currentTab !== e.data.tab;
    currentTab = e.data.tab;
    rpEdited = e.data.edited === true;
    if (switched) autoRefresh();
  }
  if (e.data.type === 'aa-theme' && (e.data.theme === 'light' || e.data.theme === 'dark')) setTheme(e.data.theme);
  if (e.data.type === 'aa-tv') showChart(e.data);
  if (e.data.type === 'aa-edit-order') openOrderDialog(e.data);
  if (e.data.type === 'aa-tv-hide') hideChartSoon();
});

// TradingView mini chart for a ticker's chart icon in the report. Drawn here, over the report frame, because
// the sandboxed frame has no origin and TradingView's data connection refuses it. Only a symbol and the
// icon's position come from the frame; both are validated.
let chartPop = null, chartHideT = null;
function showChart(d) {
  const r = d.rect || {};
  if (typeof d.symbol !== 'string' || !/^([A-Z]{2,8}:)?[A-Z0-9.]{1,10}$/.test(d.symbol)) return;
  if (!['left', 'top', 'right', 'bottom'].every((k) => Number.isFinite(r[k]))) return;
  clearTimeout(chartHideT);
  if (!chartPop) {
    chartPop = document.createElement('div');
    chartPop.id = 'tvpop';
    chartPop.addEventListener('mouseenter', () => clearTimeout(chartHideT));
    chartPop.addEventListener('mouseleave', hideChartSoon);
    document.body.append(chartPop);
  }
  if (chartPop.dataset.symbol !== d.symbol || chartPop.hidden) {
    const cfg = { symbol: d.symbol, width: '100%', height: '100%', locale: 'en', dateRange: '3M',
      colorTheme: d.theme === 'dark' ? 'dark' : 'light', isTransparent: false, autosize: true, largeChartUrl: '' };
    const f = document.createElement('iframe');
    f.title = d.symbol + ' chart (TradingView)';
    f.referrerPolicy = 'no-referrer';
    f.src = 'https://www.tradingview-widget.com/embed-widget/mini-symbol-overview/?locale=en#' + encodeURIComponent(JSON.stringify(cfg));
    chartPop.replaceChildren(f);
    chartPop.dataset.symbol = d.symbol;
  }
  chartPop.hidden = false;
  const fr = $('frame').getBoundingClientRect();
  const w = chartPop.offsetWidth, h = chartPop.offsetHeight;
  const left = fr.left + r.left, top = fr.top + r.top, bottom = fr.top + r.bottom;
  chartPop.style.left = Math.max(8, Math.min(left, innerWidth - w - 8)) + 'px';
  chartPop.style.top = (bottom + h + 12 < innerHeight ? bottom + 6 : Math.max(8, top - h - 6)) + 'px';
}
function hideChartSoon() {
  clearTimeout(chartHideT);
  chartHideT = setTimeout(() => { if (chartPop) { chartPop.hidden = true; chartPop.replaceChildren(); delete chartPop.dataset.symbol; } }, 250);
}
addEventListener('scroll', () => { if (chartPop && !chartPop.hidden) hideChartSoon(); }, { passive: true });

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
$('refresh').addEventListener('click', () => refresh());
$('refreshAll').addEventListener('click', () => refresh(true));
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
// Fetch the engine while the user fills in the form.
if ('requestIdleCallback' in window) requestIdleCallback(() => loadEngine().catch(() => { engineReady = null; }));

// ------------------------------------------------------------------------- change an order's limit price
// Opened from a limit price in the report. The frame only names the order and passes reference prices to
// show; everything that is sent (which order, its legs and quantity, the old price) comes from the order as
// Alpaca returned it. Nothing is sent before the second, explicit confirmation.
const NOT_REPLACEABLE = { accepted: 'accepted (queued, not yet at the exchange)', pending_new: 'pending new',
  pending_cancel: 'being cancelled', pending_replace: 'already being changed' };
let ordCur = null;
const money2 = (v) => '$' + Math.abs(v).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: Math.abs(v) < 1 ? 4 : 2 });
const dc = (v, mleg) => (mleg ? `${money2(v)} ${v < 0 ? 'credit' : v > 0 ? 'debit' : 'even'}` : money2(v));
function legText(o) {
  const legs = o.legs?.length ? o.legs : [o];
  return legs.map((l) => {
    const m = OCC.exec(l.symbol || '');
    const what = m ? `${m[2].slice(2, 4)}/${m[2].slice(4, 6)} ${+m[4] / 1000}${m[3]}` : l.symbol;
    const q = +(l.ratio_qty || 1);
    return `${l.side === 'sell' ? 'sell' : 'buy'} ${q !== 1 ? q + '× ' : ''}${what}`;
  }).join(', ');
}
// A new order closing an open option position, from the report's "All option strategies" P&L. Every leg
// must close (part of) a position Alpaca lists, so the frame can't ask for anything else.
function closingOrderFrom(n) {
  if (!n || typeof n !== 'object' || !Array.isArray(n.legs) || !n.legs.length || n.legs.length > 4) return null;
  const qty = n.qty;
  if (!Number.isInteger(qty) || qty < 1 || qty > 10000) return null;
  const legs = [];
  for (const l of n.legs) {
    if (!l || typeof l.symbol !== 'string' || !Number.isInteger(l.ratio_qty) || l.ratio_qty < 1) return null;
    if (!OCC.test(l.symbol) && (n.legs.length > 1 || !/^[A-Z][A-Z0-9.]{0,9}$/.test(l.symbol))) return null;   // stock: alone
    const p = lastPositions.find((x) => x.symbol === l.symbol);
    const pq = p ? +p.qty : 0;
    const side = pq > 0 ? 'sell' : 'buy';
    if (!p || l.side !== side || l.ratio_qty * qty > Math.abs(pq) + 1e-9) return null;
    legs.push({ symbol: l.symbol, side, position_intent: side === 'sell' ? 'sell_to_close' : 'buy_to_close', ratio_qty: String(l.ratio_qty) });
  }
  const base = { id: null, synthetic: true, type: 'limit', time_in_force: 'gtc', limit_price: '0', status: 'new', qty: String(qty) };
  return legs.length > 1 ? { ...base, order_class: 'mleg', legs }
    : { ...base, order_class: 'simple', symbol: legs[0].symbol, side: legs[0].side, position_intent: legs[0].position_intent, qty: String(qty * +legs[0].ratio_qty) };
}
function openOrderDialog(d, place = false) {
  if (typeof d.id !== 'string') return;
  let o;
  if (/^new:\d{1,9}$/.test(d.id)) {
    o = closingOrderFrom(d.ref?.new);
    place = true;
  } else {
    if (!/^[0-9a-f-]{8,64}$/i.test(d.id)) return;
    o = (place ? lastCancelled : lastOrders).find((x) => x.id === d.id);
  }
  const dlg = $('ordDlg');
  $('ordErr').hidden = true; $('ordConfirm').hidden = true;
  $('ordGo').textContent = 'Review change'; $('ordGo').disabled = false; $('ordMode').hidden = true;
  if (!o || o.type !== 'limit' || o.limit_price == null) {
    $('ordWhat').textContent = 'This order is no longer open or has no limit price to change. Refresh to update the report.';
    $('ordRefs').textContent = ''; $('ordPx').closest('.ordrow').hidden = true; $('ordGo').disabled = true;
    $('ordProfitRow').hidden = true; $('ordKill').hidden = true;
    ordCur = null; dlg.showModal(); return;
  }
  $('ordPx').closest('.ordrow').hidden = false;
  const mleg = o.order_class === 'mleg';
  const und = (OCC.exec((o.legs?.[0] || o).symbol || '') || [])[1] || o.symbol;
  const qty = +o.qty;
  const lim = +o.limit_price;
  // Alpaca can't change an order in place while it is "accepted" (held until the session opens): cancel it
  // and place the same order at the new price instead, after its own confirmation
  const ref = d.ref && typeof d.ref === 'object' ? d.ref : {};
  const num = (v) => (typeof v === 'number' && Number.isFinite(v) ? v : null);
  const isOpt = (o.legs?.length ? o.legs : [o]).every((l) => OCC.test(l.symbol || ''));
  ordCur = { id: o.id, mleg, qty, lim, und, order: o, recreate: !place && o.status === 'accepted', place, maxloss: num(ref.maxloss), mult: isOpt ? 100 : 1 };
  const label = typeof ref.label === 'string' ? ref.label.slice(0, 160) : `${und} · ${legText(o)}`;
  if (o.synthetic) ordCur.closing = true;
  ordCur.overlap = o.synthetic && typeof ref.overlap === 'string' ? ref.overlap.slice(0, 300) : '';
  $('ordWhat').textContent = `${label} · ${qty} ${mleg ? (qty === 1 ? 'spread' : 'spreads') : !isOpt ? (qty === 1 ? 'share' : 'shares') : (qty === 1 ? 'contract' : 'contracts')} · ${(o.time_in_force || '').toUpperCase()} · ${o.status}` +
    (lastCreds?.env === 'paper' ? ' · paper account' : ' · LIVE account');
  const refs = [[o.synthetic ? null : place ? 'was' : 'now', mleg ? lim : (o.side === 'sell' ? -lim : lim)], ['mid', num(ref.mid)], ['natural', num(ref.natural)], ['est.', num(ref.est)]]
    .filter(([k, v]) => k !== null && v !== null).map(([k, v]) => `${k} ${dc(v, true)}`);
  $('ordRefs').textContent = refs.join(' · ');
  // a clicked natural / estimate price prefills the input (debit positive, like the report shows it)
  let pre = num(ref.prefill);
  if (pre !== null && !mleg) pre = Math.abs(pre);
  const start = pre !== null && Math.abs(pre) <= 100000 ? Math.round(pre * 100) / 100 : lim;
  $('ordPx').value = fmtPx(ordCur, start);
  $('ordSide').value = start < 0 ? 'credit' : 'debit';
  $('ordSideL').hidden = !mleg;
  $('ordPx').previousSibling.textContent = mleg ? 'New limit (per spread)' : `New limit (per contract, ${o.side === 'sell' ? 'sell' : 'buy'})`;
  setOrdMode();
  if (ordCur.overlap) {     // kept in the reference line so it stays visible while typing
    $('ordRefs').textContent = `⚠ Already being closed by another open order: ${ordCur.overlap}. Alpaca may refuse a second ` +
      'closing order for the same contracts; change or cancel that one instead (Positions tab).';
  }
  if (NOT_REPLACEABLE[o.status] && !ordCur.recreate) {
    $('ordErr').textContent = `Alpaca can't change an order while it is ${NOT_REPLACEABLE[o.status]}. Try again in a moment.`;
    $('ordErr').hidden = false;
  }
  dlg.showModal();
  // closing orders: work from a target profit too (cash at the limit − cost basis of what it closes)
  const basis = num(ref.basis), scale = num(ref.scale);
  ordCur.basis = basis !== null && scale > 0 && (!place || o.synthetic) ? basis : null;
  ordCur.scale = scale;
  $('ordProfitRow').hidden = ordCur.basis === null;
  // stock closing orders can carry a stop-loss: an OCO (take-profit limit + stop) — Alpaca has no OCO for options
  const isOco = o.order_class === 'oco';
  const stopLeg = isOco ? (o.legs || []).find((l) => l.stop_price != null) : null;
  ordCur.isOco = isOco;
  ordCur.recreate0 = ordCur.recreate;
  ordCur.stopLeg = stopLeg;
  ordCur.ocoOk = !mleg && ordCur.mult === 1 && o.type === 'limit' && Number.isInteger(+o.qty) &&
    (isOco || ordCur.basis !== null || /_to_close$/.test(o.position_intent || ''));
  $('ordSlRow').hidden = !ordCur.ocoOk;
  $('ordSlOn').checked = isOco; $('ordSlOn').disabled = isOco;
  $('ordSl').value = stopLeg ? (+stopLeg.stop_price).toString() : '';
  $('ordSlLim').value = stopLeg?.limit_price != null ? (+stopLeg.limit_price).toString() : '';
  slSync();
  $('ordKill').hidden = place;
  if (ordCur.basis !== null) syncProfit();
  // clicked Unrealized: set the limit that closes at that profit
  const target = num(ref.target);
  if (ordCur.basis !== null && target !== null) {
    $('ordProfit').value = target.toFixed(2);
    $('ordProfit').dispatchEvent(new Event('input'));
  }
  const focus = ordCur.basis !== null && ref.profit ? $('ordProfit') : $('ordPx');
  focus.focus(); focus.select();
}
// stop-loss inputs: enabled with the checkbox; the note shows the P&L at the stop against the cost basis
function slVals() {
  const c = ordCur;
  if (!c?.ocoOk || !$('ordSlOn').checked) return null;
  const sp = Number($('ordSl').value), sl = $('ordSlLim').value === '' ? null : Number($('ordSlLim').value);
  return { stop: Number.isFinite(sp) && sp > 0 ? roundPx(c, sp) : null, limit: sl !== null && Number.isFinite(sl) && sl > 0 ? roundPx(c, sl) : null };
}
function slSync() {
  const c = ordCur, on = !!c?.ocoOk && $('ordSlOn').checked;
  $('ordSl').disabled = !on; $('ordSlLim').disabled = !on;
  const v = slVals();
  let t = '';
  if (on && v?.stop && c.basis !== null) {
    const px = v.limit ?? v.stop, sgn = c.order.side === 'sell' ? 1 : -1;
    const pnl = sgn * px * c.qty - c.basis;
    t = `At the stop: ${pnl < 0 ? '−' : '+'}${money2(pnl)} vs. cost basis` + (v.limit ? ' (if it fills at the stop limit)' : ' (a stop becomes a market order: it can fill worse in a gap)');
  } else if (on) t = c.isOco ? '' : 'The order becomes an OCO: the take-profit limit above, and a stop-loss; whichever fills first cancels the other.';
  $('ordSlNote').textContent = t; $('ordSlNote').hidden = !t;
}
for (const id of ['ordSlOn', 'ordSl', 'ordSlLim']) $(id).addEventListener('input', () => { slSync(); setOrdMode(); $('ordErr').hidden = true; });
// the same stock position closed by an OCO: take-profit limit + stop-loss (stop, or stop-limit)
function ocoBody(o, tp, sl) {
  const stop = { stop_price: fmtPx(ordCur, sl.stop) };
  if (sl.limit) stop.limit_price = fmtPx(ordCur, sl.limit);
  return { symbol: o.symbol, qty: o.qty, side: o.side, type: 'limit', time_in_force: o.time_in_force === 'day' ? 'day' : 'gtc', order_class: 'oco',
    take_profit: { limit_price: fmtPx(ordCur, tp) }, stop_loss: stop };
}
function setOrdMode() {
  $('ordConfirm').hidden = true;
  if (ordCur) ordCur.killing = false;
  $('ordKill').textContent = 'Cancel order…'; $('ordKill').disabled = false;
  $('ordGo').disabled = false;
  $('ordGo').textContent = ordCur?.place ? 'Review new order' : ordCur?.recreate ? 'Review cancel & re-place' : 'Review change';
  $('ordMode').hidden = !ordCur?.recreate;
  $('ordTitle').textContent = ordCur?.closing ? 'Close this position' : ordCur?.place ? 'Place this order again' : 'Change limit price';
}
// the same order (legs, quantity, sides, time in force) at a new limit
function orderBody(o, nl) {
  const base = { type: 'limit', qty: o.qty, time_in_force: o.time_in_force, limit_price: (nl < 0 ? '-' : '') + fmtPx(ordCur, nl) };
  if (o.order_class === 'mleg') {
    return { ...base, order_class: 'mleg',
      legs: o.legs.map((l) => ({ symbol: l.symbol, ratio_qty: l.ratio_qty, side: l.side, position_intent: l.position_intent })) };
  }
  return { ...base, symbol: o.symbol, side: o.side, position_intent: o.position_intent };
}
// Options buying power an order at limit `p` ties up: the premium for a debit, and at least its max loss
// (the max loss moves dollar for dollar with the price). null when it can't be told (undefined risk).
function bpNeed(c, p) {
  if (c.closing || c.basis !== null) return 0;     // an order that only closes positions frees buying power
  const q = c.qty * c.mult;
  const signed = (v) => (c.mleg ? v : (c.order.side === 'sell' ? -v : v));      // debit positive
  const debit = Math.max(0, signed(p)) * q;
  const allBuy = (c.order.legs?.length ? c.order.legs : [c.order]).every((l) => l.side === 'buy');
  if (c.maxloss === null) return signed(p) >= 0 && (allBuy || !c.mleg) && c.order.side !== 'sell' ? debit : null;
  const lossAt = c.maxloss - (signed(p) - signed(c.lim)) * q;                   // negative = a loss
  return Math.max(debit, -Math.min(0, lossAt));
}
// price decimals: stocks under $1 trade in hundredths of a cent, everything else in cents
const pxDec = (c, v) => (c && c.mult === 1 && Math.abs(v) < 1 ? 4 : 2);
const roundPx = (c, v) => { const k = 10 ** pxDec(c, v); return Math.round(v * k) / k; };
const fmtPx = (c, v) => Math.abs(v).toFixed(pxDec(c, v));
function newLimit() {
  const v = Number($('ordPx').value);
  if (!Number.isFinite(v) || v < 0 || v > 100000) return null;
  const r = roundPx(ordCur, v);
  return ordCur.mleg && $('ordSide').value === 'credit' ? -r : r;
}
for (const id of ['ordPx', 'ordSide']) $(id).addEventListener('input', () => { setOrdMode(); $('ordErr').hidden = true; syncProfit(); });
// signed (debit positive) <-> what the inputs hold
const signedOf = (c, nl) => (c.mleg ? nl : (c.order.side === 'sell' ? -nl : nl));
function syncProfit() {          // price typed: show the profit it gives
  const c = ordCur;
  if (!c || c.basis === null) return;
  const nl = newLimit();
  $('ordProfit').value = nl === null ? '' : (-signedOf(c, nl) * c.scale - c.basis).toFixed(2);
  $('ordProfitNote').textContent = '';
}
$('ordProfit').addEventListener('input', () => {      // profit typed: set the price that gives it
  const c = ordCur;
  if (!c || c.basis === null) return;
  setOrdMode(); $('ordErr').hidden = true;
  const want = Number($('ordProfit').value);
  if ($('ordProfit').value === '' || !Number.isFinite(want)) { $('ordProfitNote').textContent = ''; return; }
  const net = roundPx(c, -(want + c.basis) / c.scale);      // signed, debit positive
  if (!c.mleg && (c.order.side === 'sell' ? net > 0 : net < 0)) {
    $('ordProfitNote').textContent = `needs a ${c.order.side === 'sell' ? 'negative sale' : 'negative'} price: not possible`;
    return;
  }
  $('ordPx').value = fmtPx(c, net);
  $('ordSide').value = net < 0 ? 'credit' : 'debit';
  const got = -net * c.scale - c.basis;
  $('ordProfitNote').textContent = `→ ${dc(net, c.mleg)}` + (Math.abs(got - want) >= 0.005 ? ` (profit ${got < 0 ? '−' : ''}${money2(got)} after rounding to the cent)` : '');
});

// Cancel the order outright (two clicks)
$('ordKill').addEventListener('click', async () => {
  const c = ordCur;
  if (!c || c.place || !lastCreds) return;
  if (!c.killing) {
    c.killing = true;
    $('ordConfirm').textContent = `Cancel this order at Alpaca (limit ${dc(c.lim, c.mleg)}, quantity ${c.qty}, ${(c.order.time_in_force || '').toUpperCase()})? ` +
      'It will be listed under "Cancelled in the last day", where you can place it again.';
    $('ordConfirm').hidden = false; $('ordErr').hidden = true;
    $('ordKill').textContent = 'Confirm: cancel order';
    return;
  }
  $('ordKill').disabled = true; $('ordGo').disabled = true; $('ordKill').textContent = 'Cancelling…';
  const api = new Alpaca(lastCreds);
  try {
    await api.cancelOrder(c.id);
  } catch (e) {
    showOrdErr(`Alpaca refused to cancel the order (${e.message}).`);
    $('ordKill').disabled = false; $('ordGo').disabled = false; $('ordKill').textContent = 'Confirm: cancel order';
    return;
  }
  let st = '';
  for (let i = 0; i < 10; i++) {
    try { st = (await api.getOrder(c.id))?.status || ''; } catch (_) { st = ''; }
    if (['canceled', 'filled', 'expired'].includes(st)) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  await afterOrderChange(st === 'canceled' ? `· ${c.und} order cancelled` : `· ${c.und} cancel requested (status ${st || 'unknown'})`, st !== 'canceled');
});
$('ordCancel').addEventListener('click', () => $('ordDlg').close());
async function afterOrderChange(msg, err) {
  $('ordDlg').close();
  $('refreshStatus').classList.toggle('err', !!err);
  $('refreshStatus').textContent = msg + '; refreshing…';
  await new Promise((r) => setTimeout(r, 800));       // let Alpaca list the new order
  await refresh();
  if (!$('refreshStatus').classList.contains('err') || err) {
    $('refreshStatus').textContent = msg;
    $('refreshStatus').classList.toggle('err', !!err);
  }
}
const showOrdErr = (t) => { $('ordErr').textContent = t; $('ordErr').hidden = false; };
$('ordGo').addEventListener('click', async () => {
  if (!ordCur || !lastCreds) return;
  const nl = newLimit();
  if (nl === null) { showOrdErr('Enter a price, e.g. 2.45.'); return; }
  const c = ordCur;
  if (c.killing) setOrdMode();             // the cancel confirmation was showing: start this review afresh
  if ($('ordConfirm').hidden) {           // first click: show exactly what will be sent
    const sl = slVals();
    const slOld = c.stopLeg ? { stop: +c.stopLeg.stop_price, limit: c.stopLeg.limit_price != null ? +c.stopLeg.limit_price : null } : null;
    const slChanged = !!sl && (!slOld || sl.stop !== slOld.stop || (sl.limit ?? null) !== (slOld.limit ?? null));
    if (sl) {
      if (!sl.stop) { showOrdErr('Enter a stop price, or untick Stop-loss.'); return; }
      const sell = c.order.side === 'sell';
      if (sell ? !(sl.stop < nl) : !(sl.stop > nl)) { showOrdErr(`The stop must be ${sell ? 'below' : 'above'} the take-profit limit.`); return; }
      if (sl.limit && (sell ? sl.limit > sl.stop : sl.limit < sl.stop)) { showOrdErr(`The stop limit must be at or ${sell ? 'below' : 'above'} the stop price.`); return; }
    }
    c.sl = sl; c.slChanged = slChanged;
    // turning a plain order into an OCO: cancel it and place the OCO (Alpaca can't convert in place)
    c.convert = !!sl && !c.isOco && !c.place;
    c.recreate = c.recreate0 || c.convert;
    if (nl === c.lim && !c.place && !slChanged) { showOrdErr('That is the current limit.'); return; }
    // buying power: the old order's share is released when it is replaced or cancelled
    $('ordGo').disabled = true;
    let bpNote = '';
    try {
      const acct = await new Alpaca(lastCreds).account();
      const bp = +acct.options_buying_power;
      const need = bpNeed(c, nl), held = c.place ? 0 : (bpNeed(c, c.lim) || 0);
      if (Number.isFinite(bp) && need !== null) {
        const avail = bp + held;
        if (need > avail + 0.005) {
          showOrdErr(`Not enough options buying power: at ${dc(nl, c.mleg)} this order needs about ${money2(need)}, ` +
            `and ${money2(avail)} is available` + (held ? ` (${money2(bp)} free + ${money2(held)} this order holds now)` : '') + '. Nothing was sent.');
          $('ordGo').disabled = false;
          return;
        }
        bpNote = ` Buying power: needs about ${money2(need)} of ${money2(avail)} available.`;
      } else if (need === null) {
        bpNote = ' Buying power for this undefined-risk order can\'t be checked here; Alpaca will decide.';
      }
    } catch (e) {
      bpNote = ' (Could not check buying power: ' + (e.message || e) + ')';
    }
    $('ordGo').disabled = false;
    const tot = Math.abs(nl) * c.qty * c.mult;
    const sideTot = c.mleg ? (nl < 0 ? `receive ${money2(tot)}` : nl > 0 ? `pay ${money2(tot)}` : 'no premium') : `${money2(tot)}`;
    const what = `${dc(c.lim, c.mleg)} → ${dc(nl, c.mleg)} (${c.mult === 1 ? c.qty + ' sh' : c.qty + ' × 100'}: ${sideTot} if it fills, before fees)`;
    const slTxt = c.sl ? ` with a stop-loss at ${money2(c.sl.stop)}${c.sl.limit ? ` (stop-limit ${money2(c.sl.limit)})` : ''} (OCO: whichever fills first cancels the other)` : '';
    $('ordConfirm').textContent = (c.sl ? '' : '') + (c.convert
      ? `Cancel this order, then place an OCO for the same ${c.qty} sh: take-profit ${money2(nl)}${slTxt}. ` +
        'Two separate steps: if Alpaca rejects the OCO, the original order is placed again.' + bpNote
      : c.closing && c.sl
      ? `Place a new GTC OCO closing order for ${c.qty} sh: take-profit ${money2(nl)}${slTxt}.` + bpNote
      : c.isOco && !c.recreate
      ? `Change the OCO: take-profit ${dc(c.lim, false)} → ${money2(nl)}` + (c.slChanged ? `; stop-loss → ${money2(c.sl.stop)}${c.sl.limit ? ` (limit ${money2(c.sl.limit)})` : ''}` : '') + '.' + bpNote
      : '') || (c.closing
      ? `Place a new GTC closing order for these legs at ${dc(nl, c.mleg)} (${c.mult === 1 ? c.qty + ' sh' : c.qty + ' × 100'}: ${sideTot} if it fills, before fees).` + bpNote
      : c.place
      ? `Place a new order: the same legs, quantity and ${(c.order.time_in_force || '').toUpperCase()} as the cancelled one, at ${dc(nl, c.mleg)} ` +
        `(${c.mult === 1 ? c.qty + ' sh' : c.qty + ' × 100'}: ${sideTot} if it fills, before fees).` + (c.order.time_in_force === 'gtc' ? ' A new GTC order gets a fresh 90 days.' : '') + bpNote
      : c.recreate
      ? `Cancel this order, then place the same order (same legs, quantity and ${(c.order.time_in_force || '').toUpperCase()}) at the new limit: ${what}. ` +
        'Two separate steps: if Alpaca rejects the new order, the original order is placed again at its old price.' + bpNote
      : `Replace the order's limit ${what}. Alpaca cancels the old order and places the new one at once.` + bpNote);
    $('ordConfirm').hidden = false;
    $('ordGo').textContent = c.place ? 'Confirm: place order' : c.recreate ? 'Confirm cancel & re-place' : 'Confirm change';
    return;
  }
  $('ordGo').disabled = true; $('ordGo').textContent = 'Sending…';
  const api = new Alpaca(lastCreds);
  if (c.place) {
    try {
      const res = await api.placeOrder(c.sl ? ocoBody(c.order, nl, c.sl) : orderBody(c.order, nl));
      await afterOrderChange(`· ${c.und} ${c.closing ? (c.sl ? 'OCO closing order placed' : 'closing order placed') : 'order placed again'} at ${dc(nl, c.mleg)} (Alpaca status: ${res?.status || 'sent'})`);
    } catch (e) {
      showOrdErr(`Alpaca rejected the order (${e.message}). Nothing was placed.`);
      $('ordGo').disabled = false; $('ordGo').textContent = 'Confirm: place order';
    }
    return;
  }
  if (!c.recreate) {
    try {
      let res = null;
      if (nl !== c.lim) res = await api.replaceOrder(c.id, { limit_price: (nl < 0 ? '-' : '') + fmtPx(c, nl) });
      if (c.isOco && c.slChanged && c.stopLeg?.id) {
        const body = { stop_price: fmtPx(c, c.sl.stop) };
        if (c.sl.limit) body.limit_price = fmtPx(c, c.sl.limit);
        await api.replaceOrder(c.stopLeg.id, body);
      }
      await afterOrderChange(`· ${c.und} order changed to ${dc(nl, c.mleg)} (Alpaca status: ${res?.status || 'sent'})`);
    } catch (e) {
      if (e.status === 422 && /accepted/i.test(e.message)) {     // became (or still is) "accepted": offer cancel & re-place
        c.recreate = c.recreate0 = true; setOrdMode();
        showOrdErr(`Alpaca can't change it in place: ${e.message}. You can cancel it and place it again at the new price instead.`);
        return;
      }
      showOrdErr(`Alpaca refused the change (${e.message}). The order is unchanged.`);
      $('ordGo').disabled = false; $('ordGo').textContent = 'Confirm change';
    }
    return;
  }
  // cancel & re-place
  try {
    await api.cancelOrder(c.id);
  } catch (e) {
    showOrdErr(`Alpaca refused to cancel the order (${e.message}). Nothing was changed.`);
    $('ordGo').disabled = false; $('ordGo').textContent = 'Confirm cancel & re-place';
    return;
  }
  $('ordGo').textContent = 'Cancelling…';
  let st = '';
  for (let i = 0; i < 20; i++) {         // wait for the cancel to be confirmed before placing the new order
    try { st = (await api.getOrder(c.id))?.status || ''; } catch (_) { st = ''; }
    if (['canceled', 'filled', 'expired', 'rejected'].includes(st)) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  if (st === 'filled') { await afterOrderChange(`· ${c.und} order filled before it could be cancelled; nothing new was placed`, true); return; }
  if (st !== 'canceled' && st !== 'expired') {
    await afterOrderChange(`· ${c.und} cancel requested but not confirmed (status ${st || 'unknown'}); nothing new was placed. Check the order in Alpaca`, true);
    return;
  }
  $('ordGo').textContent = 'Placing…';
  try {
    const res = await api.placeOrder(c.sl ? ocoBody(c.order, nl, c.sl) : orderBody(c.order, nl));
    await afterOrderChange(`· ${c.und} order cancelled and placed again${c.convert ? ' as an OCO' : ''} at ${dc(nl, c.mleg)} (Alpaca status: ${res?.status || 'sent'})`);
  } catch (e) {
    // put the original order back at its old price
    $('ordGo').textContent = 'Restoring…';
    try {
      const old = c.stopLeg ? { stop: +c.stopLeg.stop_price, limit: c.stopLeg.limit_price != null ? +c.stopLeg.limit_price : null } : null;
      await api.placeOrder(c.isOco && old ? ocoBody(c.order, c.lim, old) : orderBody(c.order, c.lim));
      await afterOrderChange(`· ${c.und}: Alpaca rejected the new price (${e.message}); the original order was placed again at ${dc(c.lim, c.mleg)}`, true);
    } catch (e2) {
      await afterOrderChange(`· ${c.und}: the old order was cancelled, Alpaca rejected the new one (${e.message}) and placing the original again failed too (${e2.message}). You have no order for it now`, true);
    }
  }
});

// ------------------------------------------------------------------- recently cancelled orders: Re-place
const legSig = (o) => (o.legs?.length ? o.legs : [o]).map((l) => `${l.symbol}:${l.side}:${l.ratio_qty || 1}`).sort().join(',') + '×' + o.qty;
async function recentCancels(api, open) {
  try {
    const after = new Date(Date.now() - 24 * 3600e3).toISOString();
    const closed = await api.closedOrders(after);
    const openSigs = new Set(open.map(legSig));
    const seen = new Set(), out = [];
    for (const o of closed || []) {
      const legs = o.legs?.length ? o.legs : [o];
      if (o.status !== 'canceled' || o.type !== 'limit' || o.limit_price == null || +o.filled_qty > 0) continue;
      if (!legs.every((l) => OCC.test(l.symbol || ''))) continue;      // option orders only
      const sig = legSig(o);
      if (openSigs.has(sig) || seen.has(sig)) continue;                 // already placed again (or newer copy)
      seen.add(sig);
      out.push(o);
    }
    return out;
  } catch (_) { return []; }
}
function renderCancelled() {
  const box = $('cancelled');
  box.textContent = '';
  box.hidden = !lastCancelled.length;
  if (!lastCancelled.length) return;
  box.append('Cancelled in the last day: ');
  lastCancelled.forEach((o, i) => {
    const und = (OCC.exec((o.legs?.[0] || o).symbol) || [])[1];
    const mleg = o.order_class === 'mleg';
    const lim = +o.limit_price;
    const span = document.createElement('span');
    span.className = 'cxl';
    span.textContent = `${i ? ' · ' : ''}${und} ${o.qty}× ${legText(o)} at ${dc(mleg ? lim : (o.side === 'sell' ? -lim : lim), true)} ${(o.time_in_force || '').toUpperCase()} `;
    const b = document.createElement('button');
    b.type = 'button'; b.className = 'linkbtn'; b.textContent = 'Re-place…';
    b.onclick = () => openOrderDialog({ id: o.id, ref: {} }, true);
    span.append(b);
    box.append(span);
  });
}
