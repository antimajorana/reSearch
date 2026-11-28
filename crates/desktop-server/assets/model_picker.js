// The composer's single model dropdown: one <optgroup> per connected provider, filled from
// GET /api/models. Choosing an entry sets both provider and model for the next message.
// Model ids and labels come from providers: they are only ever set as option text/values
// (textContent), never as HTML. localStorage holds only the last choice (no secret).
// A session whose first turn has started is bound to its route (the Claude subscription
// CLI, the Antigravity CLI, or the API route): the other routes' groups are disabled and
// never picked, so a send the server would refuse (409) is never attempted.
import * as api from '/assets/api.js';

const CONNECT_VALUE = '__connect__';
const REFRESH_VALUE = '__refresh__';
const PLACEHOLDER_VALUE = '__none__';
const CLI_ROUTE_ID = 'claude-cli';
const AGY_ROUTE_ID = 'antigravity';
// Ledger route of a session's first start -> the `route` of the provider groups it may use.
const LEDGER_ROUTES = { 'claude-code-cli': CLI_ROUTE_ID, antigravity: AGY_ROUTE_ID };
const GLOBAL_KEY = 're-search-model-choice';
const SESSION_KEY = 're-search-model-by-session';
const KNOWN_MODELS_KEY = 're-search-known-models';
const MAX_REMEMBERED_SESSIONS = 200;

let selectEl = null;
let noteEl = null;
let callbacks = {};
let groups = [];
let loadedAt = 0;
let ttlMs = 10 * 60 * 1000;
let choice = null; // { provider, model }
let sessionId = null;
let sessionRouteNote = '';
let loading = null;
let loadRevision = 0;
let connectBtn = null;
let newModelKeys = new Set();
let pollTimer = null;
// Route the active session is bound to: CLI_ROUTE_ID, AGY_ROUTE_ID, 'api', or null.
let routeLock = null;

// The route a provider group runs on: the server's `route` field ("claude-cli" or "api";
// the ChatGPT plan is a subscription on the in-core "api" route), else derived as before.
function routeOf(g) {
  if (g && typeof g.route === 'string' && g.route) return g.route;
  return g?.id === CLI_ROUTE_ID || g?.id === AGY_ROUTE_ID ? g.id : 'api';
}

// Whether a provider group may be used in the active session.
function allowed(g) {
  if (!g || !routeLock) return true;
  return routeOf(g) === routeLock;
}

function readStore(key, fallback) {
  try {
    const raw = window.localStorage.getItem(key);
    return raw ? JSON.parse(raw) : fallback;
  } catch {
    return fallback;
  }
}

function writeStore(key, value) {
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Storage unavailable: the choice lasts for this page only.
  }
}

function validChoice(c) {
  return c && typeof c.provider === 'string' && typeof c.model === 'string' ? { provider: c.provider, model: c.model } : null;
}

function findModel(c) {
  if (!c) return null;
  for (const g of groups) {
    if (g.id !== c.provider || !allowed(g)) continue;
    for (const m of g.models || []) {
      if (m.id === c.model) return { group: g, model: m };
    }
  }
  return null;
}

function firstDefault() {
  for (const g of groups.filter(allowed)) {
    const models = g.models || [];
    const m = models.find((x) => x.default) || models[0];
    if (m) return { provider: g.id, model: m.id };
  }
  return null;
}

export function initModelPicker(opts = {}) {
  callbacks = opts;
  selectEl = document.getElementById('composer-model');
  noteEl = document.getElementById('composer-route-note');
  connectBtn = document.getElementById('composer-connect');
  if (connectBtn) connectBtn.addEventListener('click', openConnect);
  if (!selectEl) return;
  // Re-read the list when the dropdown is about to open after the cache time.
  const maybeReload = () => {
    if (Date.now() - loadedAt > ttlMs) loadModels(false);
  };
  selectEl.addEventListener('focus', maybeReload);
  selectEl.addEventListener('mousedown', maybeReload);
  window.addEventListener('focus', () => {
    if (Date.now() - loadedAt > ttlMs) loadModels(true);
  });
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && Date.now() - loadedAt > ttlMs) loadModels(true);
  });
  if (!pollTimer) {
    pollTimer = setInterval(() => {
      if (!document.hidden && loadedAt > 0 && Date.now() - loadedAt >= ttlMs) {
        loadModels(true);
      }
    }, 60_000);
  }
  selectEl.addEventListener('change', () => {
    const value = selectEl.value;
    if (value === CONNECT_VALUE) {
      render();
      openConnect();
      return;
    }
    if (value === REFRESH_VALUE) {
      render();
      loadModels(true).then(() => notify('Model lists refreshed from connected providers.'));
      return;
    }
    const [gi, mi] = value.split(':').map((n) => Number.parseInt(n, 10));
    const g = groups[gi];
    const m = g && g.models ? g.models[mi] : null;
    if (!m || !allowed(g)) {
      render();
      return;
    }
    choice = { provider: g.id, model: m.id };
    writeStore(GLOBAL_KEY, choice);
    updateNote();
    if (typeof callbacks.onModelSelected === 'function') {
      callbacks.onModelSelected({ ...choice, label: m.label || m.id });
    }
  });
  render();
}

function detectNewlyDiscoveredModels(fetchedGroups) {
  const known = readStore(KNOWN_MODELS_KEY, {});
  const nextKnown = { ...known };
  const announcements = [];
  for (const g of fetchedGroups || []) {
    const models = Array.isArray(g.models) ? g.models : [];
    if (!g.id || models.length === 0) continue;
    const prevList = Array.isArray(known[g.id]) ? known[g.id] : null;
    const ids = models.map((m) => String(m.id || '')).filter(Boolean);
    if (prevList) {
      const prevSet = new Set(prevList);
      const added = ids.filter((id) => !prevSet.has(id));
      for (const id of added) newModelKeys.add(`${g.id}::${id}`);
      if (added.length > 0 && !g.stale) {
        const preview = added.slice(0, 2).join(', ') + (added.length > 2 ? ` (+${added.length - 2} more)` : '');
        announcements.push(`${g.label || g.id}: ${preview}`);
      }
    }
    nextKnown[g.id] = Array.from(new Set([...(prevList || []), ...ids])).slice(-200);
  }
  writeStore(KNOWN_MODELS_KEY, nextKnown);
  if (announcements.length > 0) {
    notify(`New models available — ${announcements.join('; ')}`);
  }
}

// Load (or reload) the list; `refresh` asks the server to bypass its cache.
export function loadModels(refresh = false) {
  if (loading && !refresh) return loading;
  const revision = ++loadRevision;
  loading = (async () => {
    const first = loadedAt === 0;
    try {
      const data = await api.getModels(refresh);
      if (revision !== loadRevision) return;
      groups = Array.isArray(data && data.providers) ? data.providers : [];
      if (data && Number.isFinite(data.cache_ttl_s) && data.cache_ttl_s > 0) ttlMs = data.cache_ttl_s * 1000;
      loadedAt = Date.now();
      detectNewlyDiscoveredModels(groups);
      // Say so once (at start or on an explicit refresh), not on every background reload.
      for (const g of groups.filter((x) => x.error && (first || refresh))) {
        const shown = g.source === 'fallback' ? 'a built-in list' : 'the last list';
        notify(`${g.label}: could not fetch the model list (${g.error}); showing ${shown}.`);
      }
    } catch (err) {
      if (revision === loadRevision) notify(`Could not load models: ${err.message}`);
    } finally {
      if (revision === loadRevision) loading = null;
    }
    if (revision !== loadRevision) return;
    pickChoice(null);
    render();
    try {
      if (typeof document !== 'undefined' && typeof document.dispatchEvent === 'function' && typeof CustomEvent === 'function') {
        document.dispatchEvent(new CustomEvent('research:models-updated', { detail: { providers: groups } }));
      }
    } catch { /* non-DOM test harness */ }
  })();
  return loading;
}

function openConnect() {
  if (typeof callbacks.onOpenConnect === 'function') callbacks.onOpenConnect();
}

const AGY_FALLBACK_NOTE = 'Antigravity CLI · vendor-sandboxed';

// The route a session is bound to, as the server decides it: fixed once a turn has
// recorded `session.start` (route "claude-code-cli" or "antigravity" for a subscription
// CLI, else the API route). A binding without a start is provisional on the server, so it
// locks nothing.
function lockOf(sess) {
  const events = Array.isArray(sess && sess.events) ? sess.events : [];
  const started = events.some((e) => e && (e.kind === 'session.start' || e.kind === 'session.resume'))
    || (Array.isArray(sess && sess.messages) && sess.messages.length > 0);
  if (!started) return null;
  return LEDGER_ROUTES[sess.route] || 'api';
}

// Called when a session is selected (and again after each of its turns): its remembered
// model, else what the server stored for it, else the last choice overall, always on the
// session's route.
export function applySession(id, sess) {
  sessionId = id || null;
  routeLock = lockOf(sess);
  const meta = (sess && sess.meta) || {};
  const remembered = sessionId ? validChoice(readStore(SESSION_KEY, {})[sessionId]) : null;
  // What the server stored counts only once the session has been used: a fresh session
  // keeps the model already chosen in the dropdown.
  const used = Array.isArray(sess && sess.messages) && sess.messages.length > 0;
  const fromMeta = used && meta.default_provider && meta.default_model
    ? { provider: String(meta.default_provider), model: String(meta.default_model) }
    : null;
  const routeGroup = meta.route ? groups.find((g) => g.id === meta.route) : null;
  const isAgy = routeLock === AGY_ROUTE_ID || meta.route === AGY_ROUTE_ID || (sess && sess.route === AGY_ROUTE_ID);
  sessionRouteNote = routeGroup && routeGroup.session_note
    ? String(routeGroup.session_note)
    : (isAgy ? String((sess && sess.route_label) || (meta && meta.route_label) || AGY_FALLBACK_NOTE) : '');
  // A provisional binding (meta.route without a started turn) is only a preference; the
  // route lock above is enforced by findModel/firstDefault for every candidate.
  const onRoute = (c) => (c && (!meta.route || c.provider === meta.route) ? c : null);
  pickChoice(onRoute(remembered) || onRoute(fromMeta) || remembered || fromMeta);
  render();
}

function pickChoice(preferred) {
  const candidates = [validChoice(preferred), choice, validChoice(readStore(GLOBAL_KEY, null))];
  choice = candidates.find((c) => findModel(c)) || firstDefault();
}

// Provider and model for the next message, or null when nothing usable is connected.
export function getModelChoice() {
  return choice && findModel(choice) ? { ...choice } : null;
}

export function getConnectedModelGroups() {
  return groups.map((g) => ({ ...g, models: Array.isArray(g.models) ? [...g.models] : [] }));
}

// Why getModelChoice() is null when models exist but none fits the session's route
// (null when the reason is simply that nothing is connected).
export function noModelReason() {
  if (getModelChoice() || !routeLock) return null;
  if (!groups.some((g) => (g.models || []).length > 0)) return null;
  if (routeLock === CLI_ROUTE_ID) {
    return 'This session runs on your Claude subscription (Claude Code), which is not available right now. Start a new session to use another model.';
  }
  if (routeLock === AGY_ROUTE_ID) {
    return 'This session uses the Antigravity CLI. Check its availability under Settings → Providers, or start a new session to use another model.';
  }
  return "This session runs in re:Search's own loop (an API provider or your ChatGPT plan), and none is connected right now. Start a new session to use a subscription CLI.";
}

// Remember the model a message was sent with, for this session.
export function rememberForSession(id, c) {
  if (!id || !c) return;
  const map = readStore(SESSION_KEY, {});
  delete map[id];
  map[id] = { provider: c.provider, model: c.model };
  const keys = Object.keys(map);
  for (const k of keys.slice(0, Math.max(0, keys.length - MAX_REMEMBERED_SESSIONS))) delete map[k];
  writeStore(SESSION_KEY, map);
  writeStore(GLOBAL_KEY, map[id]);
}

function render() {
  if (!selectEl) return;
  selectEl.replaceChildren();
  const hasModels = groups.some((g) => (g.models || []).length > 0);
  const current = findModel(choice);
  if (connectBtn) connectBtn.hidden = hasModels;
  if (!hasModels || !current) {
    // A disabled placeholder holds the selection, so choosing the Connect entry below is
    // a real change; the Connect button beside the dropdown opens the panel as well.
    const placeholder = new Option(hasModels ? 'No model for this session' : 'No model connected', PLACEHOLDER_VALUE);
    placeholder.disabled = true;
    placeholder.selected = true;
    selectEl.append(placeholder);
  }
  selectEl.classList.toggle('is-empty', !hasModels);
  groups.forEach((g, gi) => {
    const models = g.models || [];
    if (models.length === 0) return;
    const og = document.createElement('optgroup');
    const name = String(g.label || g.id);
    const usable = allowed(g);
    og.label = !usable ? `${name} (not available in this session)` : g.stale ? `${name} (saved list)` : name;
    og.disabled = !usable;
    models.forEach((m, mi) => {
      const isNew = newModelKeys.has(`${g.id}::${m.id}`);
      const label = `${String(m.label || m.id)}${isNew ? ' ✦ New' : ''}`;
      const opt = new Option(label, `${gi}:${mi}`);
      opt.title = String(m.id);
      if (current && current.group === g && current.model === m) opt.selected = true;
      og.append(opt);
    });
    selectEl.append(og);
  });
  const more = document.createElement('optgroup');
  more.label = 'More';
  if (hasModels) more.append(new Option('Refresh model lists...', REFRESH_VALUE));
  more.append(new Option(hasModels ? 'Connect another provider...' : 'Open Connect...', CONNECT_VALUE));
  selectEl.append(more);
  if (!current) selectEl.value = PLACEHOLDER_VALUE;
  updateNote();
}

function updateNote() {
  const current = findModel(choice);
  const route = current ? routeOf(current.group) : routeLock;
  const cli = route === CLI_ROUTE_ID || route === AGY_ROUTE_ID;
  for (const id of ['review-mode-options', 'review-agent-options']) {
    const controls = document.getElementById(id);
    if (controls) controls.hidden = cli;
  }
  const reviewNote = document.getElementById('cli-review-note');
  if (reviewNote) reviewNote.hidden = !cli;
  if (!noteEl) return;
  const note = (current && current.group.session_note) || sessionRouteNote;
  noteEl.textContent = note ? String(note) : '';
  noteEl.hidden = !note;
}

function notify(msg) {
  if (typeof callbacks.onNotify === 'function') callbacks.onNotify(msg);
}
