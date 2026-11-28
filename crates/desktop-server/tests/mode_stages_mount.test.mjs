// Integration tests for `mode_stages.js` mounting `solve_setup.js` (S4/S5) and `idea.js` (X3/X4),
// rapid session-switch race protection, `new_menu.js` default `onCreated` wiring, and
// `project_library.js` mounting `library_send_back.js` (F13c).
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { createModeStagesController } from '../assets/mode_stages.js';
import { MODE_MENU_ENTRIES, NOT_BUILT_YET_LABEL, mountExploreCard } from '../assets/new_menu.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const crateRoot = path.resolve(__dirname, '..');
const assetsDir = path.join(crateRoot, 'assets');

async function importSessionRail() {
  const mathUrl = JSON.stringify(pathToFileURL(path.join(assetsDir, 'math.js')).href);
  const src = fs
    .readFileSync(path.join(assetsDir, 'session_rail.js'), 'utf8')
    .replace("'/assets/math.js'", mathUrl);
  return import(`data:text/javascript;base64,${Buffer.from(src).toString('base64')}`);
}

function makeStageDom() {
  let innerHtmlWrites = 0;
  const byId = new Map();

  const makeEl = (tag = 'div', ns = null) => {
    let text = '';
    let html = '';
    const attrs = new Map();
    const classes = new Set();
    const listeners = new Map();
    const el = {
      tagName: String(tag).toUpperCase(),
      namespaceURI: ns,
      id: '',
      ownerDocument: null,
      parentNode: null,
      dataset: {},
      style: {},
      children: [],
      options: [],
      type: '',
      value: '',
      placeholder: '',
      title: '',
      hidden: false,
      disabled: false,
      checked: false,
      selected: false,
      rows: 0,
      tabIndex: 0,
      get firstChild() { return this.children[0] || null; },
      get firstElementChild() { return this.children[0] || null; },
      get className() { return [...classes].join(' '); },
      set className(v) {
        classes.clear();
        for (const c of String(v || '').split(/\s+/).filter(Boolean)) classes.add(c);
      },
      classList: {
        add: (...cs) => cs.forEach((c) => classes.add(c)),
        remove: (...cs) => cs.forEach((c) => classes.delete(c)),
        toggle: (c, force) => {
          const on = force === undefined ? !classes.has(c) : Boolean(force);
          if (on) classes.add(c); else classes.delete(c);
          return on;
        },
        contains: (c) => classes.has(c),
      },
      get textContent() {
        return this.children.length ? this.children.map((c) => c.textContent ?? '').join('') : text;
      },
      set textContent(v) { this.children = []; text = String(v ?? ''); html = ''; },
      get innerHTML() { return html; },
      set innerHTML(v) { innerHtmlWrites += 1; this.children = []; html = String(v ?? ''); },
      setAttribute(k, v) {
        attrs.set(String(k), String(v));
        if (k === 'id') { this.id = String(v); byId.set(this.id, this); }
        if (k === 'class') this.className = v;
      },
      getAttribute(k) { return attrs.has(String(k)) ? attrs.get(String(k)) : null; },
      removeAttribute(k) { attrs.delete(String(k)); },
      append(...nodes) {
        for (const n of nodes) {
          if (n && typeof n === 'object') {
            if (n.parentNode && typeof n.remove === 'function') n.remove();
            n.parentNode = this;
          }
          this.children.push(n);
          if (this.tagName === 'SELECT' && n?.tagName === 'OPTION' && (!this.value || n.selected)) this.options.push(n);
        }
      },
      prepend(...nodes) {
        for (const n of nodes) if (n && typeof n === 'object') n.parentNode = this;
        this.children.unshift(...nodes);
      },
      appendChild(n) { this.append(n); return n; },
      removeChild(n) {
        const idx = this.children.indexOf(n);
        if (idx >= 0) { this.children.splice(idx, 1); if (n && typeof n === 'object') n.parentNode = null; }
        return n;
      },
      remove() {
        if (this.parentNode && typeof this.parentNode.removeChild === 'function') this.parentNode.removeChild(this);
      },
      replaceChildren(...nodes) {
        for (const c of this.children) if (c && typeof c === 'object') c.parentNode = null;
        this.children = [];
        this.options = [];
        text = '';
        html = '';
        this.append(...nodes);
      },
      addEventListener(type, fn) {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type).push(fn);
      },
      async dispatchAsync(type, event = {}) {
        const ev = { target: this, preventDefault() {}, stopPropagation() {}, ...event };
        for (const fn of listeners.get(type) || []) await fn(ev);
      },
      dispatch(type, event = {}) {
        const ev = { target: this, preventDefault() {}, stopPropagation() {}, ...event };
        for (const fn of listeners.get(type) || []) fn(ev);
      },
      querySelector(sel) { return this.querySelectorAll(sel)[0] || null; },
      querySelectorAll(sel) {
        const out = [];
        const match = (n) => {
          if (!n || !n.tagName) return false;
          if (sel.startsWith('#')) return n.id === sel.slice(1);
          if (sel.startsWith('.')) return n.classList.contains(sel.slice(1));
          const attr = sel.match(/^([a-zA-Z0-9_-]+)?\[([^=\]]+)(?:="?([^"\]]*)"?)?\]$/);
          if (attr) {
            if (attr[1] && n.tagName !== attr[1].toUpperCase()) return false;
            const val = n.getAttribute(attr[2]);
            return attr[3] === undefined ? val !== null : val === attr[3];
          }
          return n.tagName === sel.toUpperCase();
        };
        const visit = (n) => {
          for (const c of n.children || []) {
            if (match(c)) out.push(c);
            visit(c);
          }
        };
        visit(this);
        return out;
      },
    };
    return el;
  };

  const head = makeEl('head');
  const shell = makeEl('div');
  shell.setAttribute('id', 'app-shell');
  const homeExplore = makeEl('div');
  homeExplore.setAttribute('id', 'home-explore-card');
  homeExplore.hidden = true;
  const exploreStage = makeEl('div');
  exploreStage.setAttribute('id', 'explore-session-stage');
  exploreStage.hidden = true;
  const solveStage = makeEl('div');
  solveStage.setAttribute('id', 'solve-session-stage');
  solveStage.hidden = true;
  const activeTitle = makeEl('h1');
  activeTitle.setAttribute('id', 'active-session-title');

  const prevDoc = globalThis.document;
  const prevStages = globalThis.__reSearchModeStages;
  const docListeners = new Map();
  const doc = {
    head,
    createElement: (tag) => { const n = makeEl(tag, null); n.ownerDocument = doc; return n; },
    createElementNS: (ns, tag) => { const n = makeEl(tag, ns); n.ownerDocument = doc; return n; },
    createTextNode: (txt) => { const n = makeEl('#text', null); n.textContent = String(txt ?? ''); n.ownerDocument = doc; return n; },
    getElementById: (id) => byId.get(id) || null,
    querySelector: (sel) => head.querySelector(sel) || shell.querySelector(sel) || null,
    addEventListener: (t, fn) => { if (!docListeners.has(t)) docListeners.set(t, []); docListeners.get(t).push(fn); },
    removeEventListener: (t, fn) => { if (docListeners.has(t)) docListeners.set(t, docListeners.get(t).filter((f) => f !== fn)); },
    dispatchEvent: (ev) => { for (const fn of docListeners.get(ev?.type) || []) fn(ev); },
  };
  for (const n of [head, shell, homeExplore, exploreStage, solveStage, activeTitle]) n.ownerDocument = doc;
  globalThis.document = doc;

  return {
    makeEl,
    byId,
    shell,
    homeExplore,
    exploreStage,
    solveStage,
    activeTitle,
    resetInnerHtmlCount() { innerHtmlWrites = 0; },
    get innerHtmlWrites() { return innerHtmlWrites; },
    restore() {
      globalThis.document = prevDoc;
      globalThis.__reSearchModeStages = prevStages;
    },
  };
}

test('index.html and assets.rs wire solve.css, #explore-session-stage, #solve-session-stage, and mode_stages.js with Solve enabled and Explore not built yet', () => {
  const indexHtml = fs.readFileSync(path.join(assetsDir, 'index.html'), 'utf8');
  const assetsRs = fs.readFileSync(path.join(crateRoot, 'src', 'assets.rs'), 'utf8');
  assert.match(indexHtml, /href="\/assets\/solve\.css"/);
  assert.match(indexHtml, /id="explore-session-stage"\s+hidden/);
  assert.match(indexHtml, /id="solve-session-stage"\s+hidden/);
  assert.match(indexHtml, /src="\/assets\/mode_stages\.js"/);
  assert.ok(assetsRs.includes('"/assets/mode_stages.js"'));

  const exploreEntry = MODE_MENU_ENTRIES.find((e) => e.id === 'explore');
  const solveEntry = MODE_MENU_ENTRIES.find((e) => e.id === 'solve');
  assert.equal(exploreEntry.built, false);
  assert.equal(solveEntry.built, true);
  assert.ok(exploreEntry.title.includes(NOT_BUILT_YET_LABEL));
  assert.ok(!solveEntry.title.includes(NOT_BUILT_YET_LABEL));
});

test('mode_stages.js mounts solve_setup.js, solve_steps.js, solve_transcribe.js, and idea.js, handles session switching, guards rapid races, and supports openNewSolveSession + mountExploreCard', async () => {
  const dom = makeStageDom();
  try {
    const calls = [];
    let pendingSolveResolve = null;

    const stages = createModeStagesController({
      isModelConnected: () => false,
      solveOpts: { debounceMs: 0 },
      fetchJson: async (method, url, body) => {
        calls.push({ method, url, body });
        if (method === 'POST' && url === '/api/sessions' && body?.mode === 'solve') {
          return { id: 's-solve-new', mode: 'solve', title: 'Solve' };
        }
        if (url === '/api/sessions/s-solve-new/solve/view') {
          return { setup: null, steps: [], handles: [], checks: [], receipts: [], model_connected: false };
        }
        if (url === '/api/sessions/s-solve-slow/solve/view') {
          return new Promise((resolve) => { pendingSolveResolve = resolve; });
        }
        if (url === '/api/sessions/s-solve-1/solve/view') {
          return {
            setup: null,
            steps: [],
            toolbox: { available: false, line: 'Toolbox: test fixture' },
            approval_grants: {
              pre_turn_approver_supported: true,
              grants: [
                { id: 'g-net', kind: 'session_capability', effect: 'network-read', call_id: 'call-9', digest: 'dig-9', label: 'Literature read' },
              ],
            },
            model_connected: false,
          };
        }
        if (url === '/api/sessions/s-solve-1/solve/prefill') {
          return { prefill: { task_kind: 'calculate', conventions: 'hbar = 1' }, model_connected: false };
        }
        if (url === '/api/sessions/s-solve-1/solve/setup') {
          return {
            setup: { ...body, revision: 1 },
            view: {
              setup: { ...body, revision: 1 },
              steps: [{ entry_id: 'e-1', summary: 'Setup conventions', details: body.conventions, badge: { label: 'Given', provenance_tag: 'you' } }],
              model_connected: false,
            },
          };
        }
        if (url === '/api/sessions/s-solve-1/approvals/call-9') {
          return { ok: true, approved: body.approved };
        }
        throw new Error(`Unexpected URL: ${method} ${url}`);
      },
      exploreDeps: {
        getExploreSession: async (sid) => ({
          id: sid,
          mode: 'explore',
          idea: {
            statement: { rev: 1, text: 'Test topological damping scaling' },
            stage: 'search',
            candidates: [],
            where_i_looked: { services: [], library: { count: 0 } },
          },
        }),
        postIdeaOp: async () => ({}),
      },
    });
    globalThis.__reSearchModeStages = stages;
    dom.resetInnerHtmlCount();

    // 1. Mount Solve session -> unhide #solve-session-stage, render solve_setup.js + solve_steps.js + solve_transcribe.js.
    await stages.syncFromRail('s-solve-1', [{ id: 's-solve-1', mode: 'solve' }]);
    assert.equal(dom.innerHtmlWrites, 0, 'mode_stages.js must never write innerHTML');
    assert.equal(dom.solveStage.hidden, false);
    assert.equal(dom.exploreStage.hidden, true);
    assert.equal(dom.shell.getAttribute('data-mode-stage'), 'solve');
    assert.ok(dom.solveStage.querySelector('[data-testid="solve-setup-card"]'));
    assert.ok(dom.solveStage.querySelector('[data-testid="solve-workspace"]'), 'solve_steps.js workspace must mount');
    assert.ok(dom.solveStage.querySelector('[data-tab-id="transcribe"]'), 'S17b transcribe tab must mount');

    await stages.getSolveView().triggerPrefill();
    assert.equal(stages.getSolveView().getState().conventions, 'hbar = 1');

    await stages.getSolveView().getApprovalCard().allowListed();
    assert.ok(calls.some((c) => c.url === '/api/sessions/s-solve-1/approvals/call-9' && c.body.approved === true));

    const stmt = dom.solveStage.querySelector('[data-testid="solve-statement-input"]');
    stmt.value = 'Compute $x^2$';
    stmt.dispatch('input');
    await new Promise((r) => setTimeout(r, 5));
    await stages.getSolveView().submitSetup();
    assert.ok(dom.solveStage.querySelector('[data-testid="solve-collapsed-bar"]'));
    assert.ok(dom.solveStage.querySelector('[data-testid="solve-workspace"]'), 'submitSetup must keep solve_steps.js workspace intact');

    // 1b. openNewSolveSession creates a new Solve session and mounts setup + workspace cleanly.
    const createdSolve = await stages.openNewSolveSession();
    assert.equal(createdSolve.id, 's-solve-new');
    assert.equal(stages.getActiveSessionId(), 's-solve-new');
    assert.equal(stages.getActiveMode(), 'solve');
    assert.ok(dom.solveStage.querySelector('[data-testid="solve-workspace"]'));
    assert.ok(dom.solveStage.querySelector('[data-tab-id="transcribe"]'));

    // 2. Rapid switch race: start loading slow Solve session, immediately switch to Explore -> slow Solve must not overwrite!
    const slowPromise = stages.syncFromRail('s-solve-slow', [
      { id: 's-solve-slow', mode: 'solve' },
      { id: 's-exp-1', mode: 'explore' },
    ]);
    await stages.syncFromRail('s-exp-1', [{ id: 's-exp-1', mode: 'explore' }]);
    pendingSolveResolve({ setup: null, steps: [], model_connected: false });
    await slowPromise;

    assert.equal(dom.solveStage.hidden, true);
    assert.equal(dom.exploreStage.hidden, false);
    assert.equal(dom.shell.getAttribute('data-mode-stage'), 'explore');
    assert.ok(dom.exploreStage.querySelector('[data-idea-shell="true"]'));
    assert.ok(dom.exploreStage.textContent.includes('Test topological damping scaling'));

    // 3. Switch via session_rail.js renderSessionRailList to Research / Investigation / null -> hides & clears both mode stages.
    const { renderSessionRailList } = await importSessionRail();
    const railEl = dom.makeEl('nav');
    renderSessionRailList(railEl, {
      activeSessionId: 's-res-1',
      sessions: [{ id: 's-res-1', mode: 'research', title: 'Oscillator' }],
    });
    await new Promise((r) => setTimeout(r, 0));
    assert.equal(dom.solveStage.hidden, true);
    assert.equal(dom.exploreStage.hidden, true);
    assert.equal(dom.solveStage.children.length, 0);
    assert.equal(dom.exploreStage.children.length, 0);
    assert.equal(dom.shell.getAttribute('data-mode-stage'), null);

    // 4. mountExploreCard default onCreated opens newly created Explore session in #explore-session-stage!
    const cardCtrl = mountExploreCard(dom.homeExplore, {
      exploreEnabled: true,
      debounceMs: 0,
      previewFn: async () => ({
        terms: ['damping'],
        first_queries: [{ provider: 'arxiv', q: 'damping' }],
      }),
      createFn: async (payload) => ({
        id: 's-exp-created',
        title: 'Created Explore Session',
        idea: {
          statement: { rev: 1, text: payload.text },
          stage: 'idea',
          candidates: [],
          where_i_looked: { services: [], library: { count: 0 } },
        },
      }),
    });
    assert.equal(dom.homeExplore.hidden, false);
    const ideaInput = dom.homeExplore.querySelector('#idea-setup-text');
    ideaInput.value = 'Explore non-Hermitian skin damping';
    await cardCtrl.runPreviewNow();
    await cardCtrl.submitExplore();
    await new Promise((r) => setTimeout(r, 0));

    assert.equal(dom.homeExplore.hidden, true);
    assert.equal(dom.exploreStage.hidden, false);
    assert.equal(dom.activeTitle.textContent, 'Created Explore Session');
    assert.ok(dom.exploreStage.textContent.includes('Explore non-Hermitian skin damping'));
  } finally {
    dom.restore();
  }
});

test('project_library.js mounts library_send_back.js (Export BibTeX + Send to Kandinsky) with click-only execution', async () => {
  const dom = makeStageDom();
  const panel = dom.makeEl('section');
  panel.setAttribute('id', 'project-sources-panel');
  const formMock = dom.makeEl('form');
  formMock.reset = () => {};
  formMock.elements = { namedItem: () => dom.makeEl('input') };
  const origCreate = globalThis.document.createElement;
  globalThis.document.createElement = (tag) => {
    const n = origCreate(tag);
    if (String(tag).toLowerCase() === 'article') {
      const origSet = Object.getOwnPropertyDescriptor(n, 'innerHTML')?.set;
      Object.defineProperty(n, 'innerHTML', {
        configurable: true,
        get() { return ''; },
        set(v) {
          if (origSet) origSet.call(n, v);
          for (const attr of ['data-edit-paper', 'data-remove-paper']) {
            const b = origCreate('button');
            b.setAttribute(attr, '');
            n.appendChild(b);
          }
        },
      });
    }
    return n;
  };
  panel.innerHTML = '';
  const search = origCreate('input');
  search.setAttribute('type', 'search');
  const addBtn = origCreate('button');
  addBtn.setAttribute('data-add-paper', '');
  const expLink = origCreate('a');
  expLink.setAttribute('data-export-bib', '');
  const cancelBtn = origCreate('button');
  cancelBtn.setAttribute('data-cancel', '');
  formMock.appendChild(cancelBtn);
  const countP = origCreate('p');
  countP.setAttribute('data-paper-count', '');
  const listDiv = origCreate('div');
  listDiv.className = 'project-library-list';
  Object.defineProperty(panel, 'innerHTML', {
    configurable: true,
    get() { return ''; },
    set() { panel.replaceChildren(search, addBtn, expLink, formMock, countP, listDiv); },
  });

  const posted = [];
  const prevFetch = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    posted.push({ url, body: JSON.parse(init.body) });
    return {
      ok: true,
      status: 200,
      text: async () => JSON.stringify(
        url.includes('export/bibtex')
          ? { ok: true, written: false, count: 1, citekeys: ['synth2025'] }
          : { ok: true, target: 'arXiv:2501.00042v2', deep_link: 'kandinsky://capture-paper?target=arXiv%3A2501.00042v2' },
      ),
    };
  };

  try {
    const moduleUrl = (code) => `data:text/javascript;base64,${Buffer.from(code).toString('base64')}`;
    const mathUrl = pathToFileURL(path.join(assetsDir, 'math.js')).href;
    const linksCode = fs.readFileSync(path.join(assetsDir, 'source_links.js'), 'utf8')
      .replace("'/assets/math.js'", JSON.stringify(mathUrl));
    const libCode = fs.readFileSync(path.join(assetsDir, 'project_library.js'), 'utf8')
      .replace("'/assets/api.js'", JSON.stringify(moduleUrl("export const getCsrfToken = () => 'tok-test';")))
      .replace("'/assets/library_send_back.js'", JSON.stringify(pathToFileURL(path.join(assetsDir, 'library_send_back.js')).href))
      .replace("'/assets/project_edits.js'", JSON.stringify(pathToFileURL(path.join(assetsDir, 'project_edits.js')).href))
      .replace("'/assets/math.js'", JSON.stringify(mathUrl))
      .replace("'/assets/source_links.js'", JSON.stringify(moduleUrl(linksCode)));

    const { initProjectLibrary, renderProjectLibrary } = await import(moduleUrl(libCode));
    initProjectLibrary();
    renderProjectLibrary({
      id: 'proj-1',
      sources: [{
        id: 'src-1',
        title: 'Synthetic Oscillator Bounds',
        url: 'https://arxiv.org/abs/2501.00042v2',
        bibtex: '@article{synth2025,\n  title={Synthetic Oscillator Bounds}\n}',
      }],
    });

    assert.equal(posted.length, 0, 'rendering library paper must fire zero network requests');
    const slot = listDiv.querySelector('.project-paper-send-back');
    assert.ok(slot, 'project_library.js must mount .project-paper-send-back');
    assert.ok(slot.textContent.includes('synth2025'));

    const exportBtn = slot.querySelector('.send-back-export-btn');
    const kandBtn = slot.querySelector('.send-back-kandinsky-btn');
    assert.equal(exportBtn.disabled, false);
    assert.equal(kandBtn.disabled, false);

    await exportBtn.dispatchAsync('click');
    await slot.querySelector('.send-back-confirm-export').dispatchAsync('click');
    assert.equal(posted.length, 1);
    assert.equal(posted[0].url, '/api/sources/export/bibtex');
    assert.deepEqual(posted[0].body.paper_keys, ['arxiv:2501.00042']);

    await kandBtn.dispatchAsync('click');
    assert.equal(posted.length, 2);
    assert.equal(posted[1].url, '/api/sources/send/kandinsky');
    assert.equal(posted[1].body.paper_key, 'arxiv:2501.00042');
    assert.equal(posted[1].body.user_initiated, true);
  } finally {
    globalThis.fetch = prevFetch;
    dom.restore();
  }
});

test('mode_stages.js populates Solver dropdown with connected models, refreshes on models-updated, and auto-starts first leg unless ask_before_running', async () => {
  const dom = makeStageDom();
  try {
    const calls = [];
    const stages = createModeStagesController({
      isModelConnected: () => true,
      listConnectedModels: () => ({
        providers: [
          { id: 'claude-cli', label: 'Claude Code', route: 'claude-cli', models: [{ id: 'sonnet', label: 'Sonnet' }] },
          {
            id: 'chatgpt',
            label: 'ChatGPT',
            route: 'api',
            models: [
              { id: 'gpt-6.2-sol', label: 'gpt-6.2-sol' },
              { id: 'gpt-5.4', label: 'gpt-5.4' },
            ],
          },
        ],
      }),
      solveOpts: { debounceMs: 0 },
      fetchJson: async (method, url, body) => {
        calls.push({ method, url, body });
        if (method === 'GET' && url.endsWith('/solve/view')) {
          return { setup: null, steps: [], handles: [], checks: [], receipts: [], model_connected: true };
        }
        if (method === 'POST' && url.endsWith('/solve/prefill')) {
          return { prefill: { task_kind: 'calculate' }, model_connected: true };
        }
        if (method === 'POST' && url.endsWith('/solve/setup')) {
          return {
            setup: { ...body, revision: 1 },
            view: { setup: { ...body, revision: 1 }, steps: [], model_connected: true },
          };
        }
        if (method === 'POST' && url.endsWith('/solve/continue')) {
          return {
            ok: true,
            accepted: true,
            view: { setup: { statement: 'Compute x^2', revision: 1 }, steps: [], run_state: { state: 'running', running: false }, model_connected: true },
          };
        }
        throw new Error(`Unexpected URL: ${method} ${url}`);
      },
    });

    await stages.syncFromRail('s-run-auto', [{ id: 's-run-auto', mode: 'solve' }]);
    const solverSelect = dom.solveStage.querySelector('[data-testid="solve-solver-select"]');
    assert.ok(solverSelect);
    assert.deepEqual(
      solverSelect.children.map((o) => o.value),
      ['', 'chatgpt::gpt-6.2-sol', 'chatgpt::gpt-5.4'],
    );
    assert.equal(solverSelect.children[0].textContent, 'Not selected');

    // Refresh model list event adds a third model without losing typed statement.
    const stmt = dom.solveStage.querySelector('[data-testid="solve-statement-input"]');
    stmt.value = 'Compute $\\int_0^1 x^2\\,dx$.';
    stmt.dispatch('input');
    globalThis.document.dispatchEvent({
      type: 'research:models-updated',
      detail: { providers: [{ id: 'openai', label: 'OpenAI', route: 'api', models: [{ id: 'gpt-4.1', label: 'gpt-4.1' }] }] },
    });
    const refreshedSel = dom.solveStage.querySelector('[data-testid="solve-solver-select"]');
    assert.deepEqual(refreshedSel.children.map((o) => o.value), ['', 'openai::gpt-4.1']);

    // Run with ask_before_running unchecked -> posts setup AND chains solve/continue; Save revision does not re-start.
    await stages.getSolveView().submitSetup();
    assert.ok(calls.some((c) => c.url === '/api/sessions/s-run-auto/solve/continue'));
    calls.length = 0;
    dom.solveStage.querySelector('[data-action="edit-setup"]').dispatch('click');
    await stages.getSolveView().submitSetup();
    assert.equal(calls.some((c) => c.url.endsWith('/solve/continue')), false, 'Save revision must not auto-continue');

    // Fresh session with ask_before_running checked -> saves setup ONLY, no solve/continue.
    calls.length = 0;
    await stages.syncFromRail('s-run-ask', [{ id: 's-run-ask', mode: 'solve' }]);
    const stmt2 = dom.solveStage.querySelector('[data-testid="solve-statement-input"]');
    stmt2.value = 'Prove $x^2 \\ge 0$';
    stmt2.dispatch('input');
    const askBox = dom.solveStage.querySelector('[data-testid="solve-ask-before-running"]');
    askBox.checked = true;
    askBox.dispatch('change');
    await stages.getSolveView().submitSetup();
    assert.ok(calls.some((c) => c.url === '/api/sessions/s-run-ask/solve/setup'));
    assert.equal(calls.some((c) => c.url.endsWith('/solve/continue')), false);
  } finally {
    dom.restore();
  }
});
