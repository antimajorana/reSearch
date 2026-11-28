// Mode-specific center-stage mounter for Solve (`solve_setup.js` + `solve_steps.js` + `solve_transcribe.js`
// + `solve_artifacts.js`) and Explore (`idea.js`, X3/X4). Keeps `app.js` untouched and keeps left-rail
// Explore disabled ("not built yet", F5) until Explore ships. Zero `innerHTML` writes; race-guarded by syncSeq.
import * as api from './api.js';
import { createExploreSessionController } from './idea.js';
import { EXPLORE_VISIBILITY_STORAGE_KEY } from './idea_state.js';
import { mountUsedLessonsDisclosure } from './lesson_remember.js';
import {
  SOLVE_ARTIFACTS_TAB,
  mountSolveArtifactsSection,
  registerSolveTab,
  renderSolveArtifacts,
} from './solve_artifacts.js';
import { createSolveSetupView } from './solve_setup.js';
import { createSolveWorkspace } from './solve_steps.js';
import { SOLVE_TRANSCRIBE_TAB } from './solve_transcribe.js';

function resolveAuthToken(apiMod) {
  const fromApi = typeof apiMod?.getCsrfToken === 'function' ? String(apiMod.getCsrfToken() || '').trim() : '';
  if (fromApi) return fromApi;
  if (typeof document !== 'undefined' && typeof document.querySelector === 'function') {
    const meta = document.querySelector('meta[name="re-search-csrf"]');
    if (meta && typeof meta.content === 'string' && meta.content.trim()) return meta.content.trim();
  }
  try {
    if (typeof window !== 'undefined' && window?.sessionStorage) {
      return String(window.sessionStorage.getItem('re-search-token') || '').trim();
    }
  } catch { /* sessionStorage unavailable */ }
  return '';
}

async function defaultFetchJson(apiMod, method, path, body) {
  const headers = {};
  const token = resolveAuthToken(apiMod);
  if (token) headers['X-ReSearch-Token'] = token;
  const init = { method, credentials: 'same-origin', headers };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const resp = await fetch(path, init);
  const text = await resp.text();
  let data = null;
  if (text) { try { data = JSON.parse(text); } catch { data = { raw: text }; } }
  if (!resp.ok) {
    const err = new Error((data && (data.error || data.message)) || `HTTP ${resp.status}`);
    err.status = resp.status;
    err.payload = data;
    throw err;
  }
  return data;
}

function ensureIdeaStylesheet() {
  if (typeof document === 'undefined' || !document.head || typeof document.querySelector !== 'function') return;
  if (document.querySelector('link[href="/assets/idea.css"]')) return;
  const link = Object.assign(document.createElement('link'), { rel: 'stylesheet', href: '/assets/idea.css' });
  document.head.appendChild(link);
}

function normalizeMode(raw) {
  const norm = String(raw || '').trim().toLowerCase();
  if (norm === 'solve' || norm === 'explore') return norm;
  return (norm === 'investigation' || norm === 'investigate') ? 'investigate' : 'research';
}

const CLI_ROUTE_PROVIDERS = new Set(['claude-cli', 'antigravity']);

export function normalizeConnectedModels(raw) {
  const list = Array.isArray(raw) ? raw : (Array.isArray(raw?.providers) ? raw.providers : []);
  const out = [];
  const seen = new Set();
  const pushItem = (provider, model, label) => {
    const p = String(provider || '').trim();
    const m = String(model || '').trim();
    if (!p || !m || CLI_ROUTE_PROVIDERS.has(p)) return;
    const key = `${p}::${m}`;
    if (seen.has(key)) return;
    seen.add(key);
    out.push({ provider: p, model: m, label: String(label || `${p} / ${m}`) });
  };
  for (const item of list) {
    if (!item || typeof item !== 'object') continue;
    if (Array.isArray(item.models)) {
      if (CLI_ROUTE_PROVIDERS.has(item.id) || CLI_ROUTE_PROVIDERS.has(item.route)) continue;
      const groupLabel = String(item.label || item.id || '').trim();
      for (const m of item.models) {
        if (!m || typeof m !== 'object') continue;
        const prov = m.provider || item.id;
        const modId = m.id || m.model;
        const modLabel = String(m.label || modId || '').trim();
        pushItem(prov, modId, groupLabel ? `${groupLabel} / ${modLabel}` : modLabel);
      }
    } else {
      pushItem(item.provider, item.model || item.id, item.label);
    }
  }
  return out;
}

function detectModelConnected(deps) {
  if (typeof deps.isModelConnected === 'function') return Boolean(deps.isModelConnected());
  const sel = typeof document !== 'undefined' && typeof document.getElementById === 'function' ? document.getElementById('composer-model') : null;
  return Boolean(sel?.options && Array.from(sel.options).some((opt) => opt && !opt.disabled && opt.value && !String(opt.value).startsWith('__')));
}

export function createModeStagesController(deps = {}) {
  const apiMod = deps.api || api;
  const customFetch = typeof deps.fetchJson === 'function';
  const fetchJson = customFetch
    ? deps.fetchJson
    : (method, path, body) => defaultFetchJson(apiMod, method, path, body);
  const modeBySession = new Map();
  const solveTabs = new Map();
  registerSolveTab(solveTabs, SOLVE_ARTIFACTS_TAB);
  registerSolveTab(solveTabs, SOLVE_TRANSCRIBE_TAB);
  let syncSeq = 0;
  let mountedSessionId = null;
  let mountedMode = null;
  let activeSolveView = null;
  let activeSolveWorkspace = null;
  let activeSolveArtifacts = null;
  let activeLessonsWidget = null;
  let activeExploreCtrl = null;
  let activeModelsCleanup = null;
  let solvePollTimer = null;
  let creatingSolve = false;
  let inFlightMount = null;

  const getEl = (id) => (typeof deps.getElementById === 'function'
    ? deps.getElementById(id)
    : (typeof document !== 'undefined' && typeof document.getElementById === 'function' ? document.getElementById(id) : null));

  function setShellStageAttr(stage) {
    const shell = getEl('app-shell');
    if (!shell) return;
    if (stage) shell.setAttribute('data-mode-stage', stage);
    else if (typeof shell.removeAttribute === 'function') shell.removeAttribute('data-mode-stage');
  }

  function clearStage(el) {
    if (!el) return;
    el.hidden = true;
    if (typeof el.replaceChildren === 'function') el.replaceChildren();
  }

  function maybeRestoreHomeExplore(activeId) {
    const homeCard = getEl('home-explore-card');
    if (!homeCard) return;
    if (activeId) { homeCard.hidden = true; return; }
    let enabled = false;
    try { enabled = typeof localStorage !== 'undefined' && localStorage?.getItem(EXPLORE_VISIBILITY_STORAGE_KEY) === '1'; } catch { enabled = false; }
    homeCard.hidden = !(enabled && (homeCard.children?.length || 0) > 0);
  }

  async function resolveGrant(sessionId, grant, approved) {
    const encSid = encodeURIComponent(sessionId);
    let callId = grant?.call_id || '';
    let digest = grant?.digest || '';
    if (!callId || !digest) {
      const pendingResp = (!customFetch && typeof apiMod.getPendingApprovals === 'function')
        ? await apiMod.getPendingApprovals(sessionId)
        : await fetchJson('GET', `/api/sessions/${encSid}/approvals`);
      const list = Array.isArray(pendingResp?.pending) ? pendingResp.pending : [];
      const match = list.find((p) => (
        (grant?.call_id && p.call_id === grant.call_id)
        || (grant?.caller && p.caller === grant.caller && (!grant.tool_id || p.tool === grant.tool_id))
        || (grant?.effect && p.effect === grant.effect)
      ));
      if (match) { callId = match.call_id; digest = match.digest; }
    }
    if (!callId || !digest) {
      return { approved: false, pending: true, reason: 'Approvals are prompted when the first turn starts.' };
    }
    const payload = { digest, approved: Boolean(approved), approve_for_session: Boolean(approved && grant?.kind === 'session_capability') };
    return (!customFetch && typeof apiMod.respondApproval === 'function')
      ? apiMod.respondApproval(sessionId, callId, payload)
      : fetchJson('POST', `/api/sessions/${encSid}/approvals/${encodeURIComponent(callId)}`, payload);
  }

  async function resolveConnectedModels() {
    if (typeof deps.listConnectedModels === 'function') {
      try { return normalizeConnectedModels(await deps.listConnectedModels()); } catch { return []; }
    }
    if (!customFetch && typeof apiMod?.getModels === 'function') {
      try { return normalizeConnectedModels(await apiMod.getModels(false)); } catch { return []; }
    }
    return [];
  }

  function destroySolveWidgets() {
    if (solvePollTimer) {
      clearTimeout(solvePollTimer);
      solvePollTimer = null;
    }
    if (activeModelsCleanup) {
      try { activeModelsCleanup(); } finally { activeModelsCleanup = null; }
    }
    if (activeSolveWorkspace) {
      try {
        activeSolveWorkspace.destroy();
      } finally {
        activeSolveWorkspace = null;
      }
    }
    if (activeSolveArtifacts) {
      try {
        activeSolveArtifacts.destroy();
      } finally {
        activeSolveArtifacts = null;
      }
    }
    if (activeLessonsWidget) {
      try {
        activeLessonsWidget.destroy();
      } finally {
        activeLessonsWidget = null;
      }
    }
  }

  function resetStages(nextSessionId = null, nextMode = null) {
    mountedSessionId = nextSessionId;
    mountedMode = nextMode;
    activeSolveView = null;
    activeExploreCtrl = null;
    destroySolveWidgets();
    setShellStageAttr(null);
    clearStage(getEl('solve-session-stage'));
    clearStage(getEl('explore-session-stage'));
    maybeRestoreHomeExplore(nextSessionId);
  }

  function mountSolveStage(sessionId, seq) {
    const promise = (async () => {
      const solveHost = getEl('solve-session-stage');
      if (!solveHost) return null;
      destroySolveWidgets();
      clearStage(getEl('explore-session-stage'));
      const homeCard = getEl('home-explore-card');
      if (homeCard) homeCard.hidden = true;
      activeExploreCtrl = null;
      setShellStageAttr('solve');
      solveHost.hidden = false;

      const encSid = encodeURIComponent(sessionId);
      const [viewData, initialModels] = await Promise.all([
        fetchJson('GET', `/api/sessions/${encSid}/solve/view`),
        resolveConnectedModels(),
      ]);
      if (seq !== syncSeq) return null;
      let connectedModels = initialModels;

      const doc = solveHost.ownerDocument || (typeof document !== 'undefined' ? document : null);
      const makeSlot = (cls) => {
        const div = doc.createElement('div');
        div.className = cls;
        return div;
      };
      const lessonsSlot = makeSlot('solve-stage-lessons-slot');
      const setupSlot = makeSlot('solve-stage-setup-slot');
      const workspaceSlot = makeSlot('solve-stage-workspace-slot');
      const artifactsSlot = makeSlot('solve-stage-artifacts-slot');
      solveHost.replaceChildren(lessonsSlot, setupSlot, workspaceSlot, artifactsSlot);

      let currentView = viewData && typeof viewData === 'object' ? viewData : {};
      const artifactOpts = {
        sessionId,
        fetchFn: deps.fetchArtifact || deps.fetch || (typeof fetch === 'function' ? fetch : null),
        getToken: () => (typeof deps.getToken === 'function' ? deps.getToken() : resolveAuthToken(apiMod)),
        urlApi: deps.urlApi,
        scheduleRevoke: deps.scheduleRevoke,
      };

      const scheduleRunningPoll = () => {
        if (solvePollTimer) { clearTimeout(solvePollTimer); solvePollTimer = null; }
        if (seq !== syncSeq || (customFetch && deps.pollIntervalMs == null)) return;
        const isRunning = Boolean(currentView?.run_state?.running || currentView?.run_state?.state === 'running');
        if (!isRunning) return;
        solvePollTimer = setTimeout(async () => {
          solvePollTimer = null;
          if (seq !== syncSeq) return;
          try {
            const polled = await fetchJson('GET', `/api/sessions/${encSid}/solve/view`);
            applyUpdatedView(polled);
          } catch { /* transient poll failure */ }
        }, deps.pollIntervalMs ?? 1000);
      };

      const applyUpdatedView = (nextView, { updateSetup = true } = {}) => {
        if (!nextView || typeof nextView !== 'object' || seq !== syncSeq) return currentView;
        currentView = { ...(currentView || {}), ...nextView };
        if (updateSetup && activeSolveView) activeSolveView.applyView(currentView);
        if (activeSolveWorkspace) activeSolveWorkspace.update(currentView);
        if (activeSolveArtifacts) activeSolveArtifacts.update(currentView);
        if (currentView?.lessons_supplied?.count > 0) {
          if (activeLessonsWidget) activeLessonsWidget.update(currentView.lessons_supplied);
          else activeLessonsWidget = mountUsedLessonsDisclosure(lessonsSlot, currentView.lessons_supplied, deps);
        }
        scheduleRunningPoll();
        return currentView;
      };

      const postAndRefresh = async (path, body = {}) => {
        const res = await fetchJson('POST', path, body);
        const nextView = res?.view || await fetchJson('GET', `/api/sessions/${encSid}/solve/view`);
        applyUpdatedView(nextView);
        if (res && typeof res === 'object') {
          return res.view ? res : { ...res, view: currentView };
        }
        return { ok: true, view: currentView };
      };

      const view = createSolveSetupView(setupSlot, {
        initial: {
          modelConnected: connectedModels.length > 0 || detectModelConnected(deps),
          connectedModels,
        },
        onPrefill: (body) => (seq === syncSeq ? fetchJson('POST', `/api/sessions/${encSid}/solve/prefill`, body) : Promise.resolve(null)),
        onSubmitSetup: async (body) => {
          const hadSetup = Boolean(currentView?.setup);
          const res = await fetchJson('POST', `/api/sessions/${encSid}/solve/setup`, body);
          if (res?.view || res?.artifacts) {
            applyUpdatedView(res.view || res, { updateSetup: false });
          }
          const hasModel = Boolean(
            res?.view?.model_connected || currentView?.model_connected || connectedModels.length > 0 || detectModelConnected(deps),
          );
          if (!hadSetup && !body?.ask_before_running && hasModel && seq === syncSeq) {
            try {
              const cont = await postAndRefresh(`/api/sessions/${encSid}/solve/continue`, {});
              return { ...(res && typeof res === 'object' ? res : {}), view: cont?.view || currentView, started: true };
            } catch { /* setup already saved */ }
          }
          return res;
        },
        onAllowGrant: (grant) => resolveGrant(sessionId, grant, true),
        onDenyGrant: (grant) => resolveGrant(sessionId, grant, false),
        ...(deps.solveOpts || {}),
      });
      if (viewData) {
        view.applyView({ ...viewData, model_connected: Boolean(viewData.model_connected || connectedModels.length > 0) });
      }
      if (typeof doc?.addEventListener === 'function') {
        const onModelsUpdated = (ev) => {
          if (seq !== syncSeq || !activeSolveView) return;
          connectedModels = normalizeConnectedModels(ev?.detail);
          activeSolveView.applyView({
            connected_models: connectedModels,
            model_connected: connectedModels.length > 0 || detectModelConnected(deps),
          });
        };
        doc.addEventListener('research:models-updated', onModelsUpdated);
        activeModelsCleanup = () => doc.removeEventListener?.('research:models-updated', onModelsUpdated);
      }
      scheduleRunningPoll();
      if (currentView?.lessons_supplied?.count > 0) {
        activeLessonsWidget = mountUsedLessonsDisclosure(lessonsSlot, currentView.lessons_supplied, deps);
      }

      const artifactsTab = {
        id: SOLVE_ARTIFACTS_TAB.id,
        label: SOLVE_ARTIFACTS_TAB.label,
        mount(panel, ctx) {
          return renderSolveArtifacts(panel, ctx.view, { ...artifactOpts, document: ctx.document });
        },
      };
      const katexImpl = deps.katex ?? (typeof globalThis !== 'undefined' ? (globalThis.window?.katex ?? globalThis.katex ?? null) : null);
      activeSolveWorkspace = createSolveWorkspace(workspaceSlot, currentView, {
        document: doc,
        sessionId,
        token: resolveAuthToken(apiMod),
        getToken: artifactOpts.getToken,
        fetch: artifactOpts.fetchFn,
        katex: katexImpl,
        extraTabs: [SOLVE_TRANSCRIBE_TAB, artifactsTab],
        reviewEnabled: true,
        onReviewModel: (step) => postAndRefresh(`/api/sessions/${encSid}/solve/review`, step?.entry_id ? { entry_id: step.entry_id } : {}),
        onAck: (_step, payload) => postAndRefresh(`/api/sessions/${encSid}/solve/ack`, payload),
        onAttachSource: (_step, payload) => postAndRefresh(`/api/sessions/${encSid}/solve/attach-source`, payload),
        onAddCheck: (_step, payload) => postAndRefresh(`/api/sessions/${encSid}/solve/check`, payload),
        onRerunChecks: (payload) => postAndRefresh(`/api/sessions/${encSid}/solve/recheck`, payload || {}),
        onTranscribe: (payload) => postAndRefresh(`/api/sessions/${encSid}/solve/transcribe`, payload),
        onContinue: (note) => postAndRefresh(
          `/api/sessions/${encSid}/solve/continue`,
          typeof note === 'string' && note ? { note } : (note && typeof note === 'object' ? note : {}),
        ),
        onRetry: () => postAndRefresh(`/api/sessions/${encSid}/solve/continue`, {}),
        onStop: () => postAndRefresh(`/api/sessions/${encSid}/stop`, {}),
        onFork: async (step) => {
          const atSeq = Number.isFinite(Number(step?.seq)) ? { at_seq: Number(step.seq) } : {};
          const res = await fetchJson('POST', `/api/sessions/${encSid}/fork`, atSeq);
          const forkId = res?.new_session || res?.id || res?.session?.id;
          if (forkId) {
            modeBySession.set(forkId, 'solve');
            const titleEl = getEl('active-session-title');
            const forkTitle = `Fork of ${titleEl?.textContent || sessionId}`;
            if (titleEl) titleEl.textContent = forkTitle;
            const rail = typeof globalThis !== 'undefined' ? globalThis.__reSearchSessionRail : null;
            const handled = rail
              ? await rail.upsertSession?.({ id: forkId, title: forkTitle, mode: 'solve', archived: false }, { select: true })
              : false;
            if (!handled && mountedSessionId !== forkId) {
              await mountSolveStage(forkId, ++syncSeq);
            }
          }
          return res;
        },
        onStepCorrected: async (payload) => {
          if (typeof deps.onStepCorrected === 'function') return deps.onStepCorrected(payload);
          if (currentView?.model_connected || detectModelConnected(deps)) {
            return postAndRefresh(`/api/sessions/${encSid}/solve/continue`, {
              correct_entry_id: payload?.entryId,
              note: payload?.correctionText || payload?.text || '',
            });
          }
          return { ok: true, queued: false };
        },
        ...(deps.solveWorkspaceOpts || {}),
      });

      activeSolveArtifacts = mountSolveArtifactsSection(artifactsSlot, currentView, artifactOpts);
      activeSolveView = view;
      mountedSessionId = sessionId;
      mountedMode = 'solve';
      return view;
    })();
    inFlightMount = { sessionId, seq, promise };
    return promise.finally(() => {
      if (inFlightMount?.promise === promise) inFlightMount = null;
    });
  }

  async function mountExploreStage(sessionId, seq, preloadedIdea = null) {
    const exploreHost = getEl('explore-session-stage');
    if (!exploreHost) return null;
    ensureIdeaStylesheet();
    destroySolveWidgets();
    clearStage(getEl('solve-session-stage'));
    const homeCard = getEl('home-explore-card');
    if (homeCard) homeCard.hidden = true;
    activeSolveView = null;
    setShellStageAttr('explore');
    exploreHost.hidden = false;

    const ctrl = createExploreSessionController(exploreHost, { hasModel: detectModelConnected(deps), ...(deps.exploreDeps || {}) });
    activeExploreCtrl = ctrl;
    mountedSessionId = sessionId;
    mountedMode = 'explore';
    if (preloadedIdea) {
      ctrl.setSessionView(sessionId, preloadedIdea, detectModelConnected(deps));
      return ctrl;
    }
    await ctrl.loadSession(sessionId, detectModelConnected(deps));
    return seq === syncSeq ? ctrl : null;
  }

  async function syncFromRail(activeSessionId, sessions = []) {
    if (Array.isArray(sessions)) {
      for (const s of sessions) if (s?.id && s?.mode) modeBySession.set(s.id, normalizeMode(s.mode));
    }
    if (!activeSessionId) { syncSeq += 1; resetStages(null, null); return null; }
    const seq = ++syncSeq;
    let mode = modeBySession.get(activeSessionId) || null;
    if (!mode) {
      try {
        const sess = (!customFetch && typeof apiMod.getSession === 'function')
          ? await apiMod.getSession(activeSessionId)
          : await fetchJson('GET', `/api/sessions/${encodeURIComponent(activeSessionId)}`);
        if (seq !== syncSeq) return null;
        mode = normalizeMode(sess?.mode);
        modeBySession.set(activeSessionId, mode);
      } catch {
        if (seq !== syncSeq) return null;
        mode = 'research';
      }
    }
    if (mode === mountedMode && activeSessionId === mountedSessionId && (mode === 'solve' || mode === 'explore')) {
      return mode === 'solve' ? activeSolveView : activeExploreCtrl;
    }
    if (mode === 'solve') {
      if (inFlightMount?.sessionId === activeSessionId) {
        try { return await inFlightMount.promise; } catch { return null; }
      }
      try { return await mountSolveStage(activeSessionId, seq); } catch { return null; }
    }
    if (mode === 'explore') { try { return await mountExploreStage(activeSessionId, seq); } catch { return null; } }
    resetStages(activeSessionId, mode);
    return null;
  }

  async function openNewSolveSession(createOpts = {}) {
    if (creatingSolve) return null;
    creatingSolve = true;
    try {
      const payload = { mode: 'solve', ...(createOpts && typeof createOpts === 'object' ? createOpts : {}) };
      const res = (!customFetch && typeof apiMod.createSession === 'function')
        ? await apiMod.createSession(payload)
        : await fetchJson('POST', '/api/sessions', payload);
      const id = res?.id || res?.session?.id || res?.session_id;
      if (!id) return null;
      modeBySession.set(id, 'solve');
      const title = String(res?.title || res?.session?.title || 'Solve');
      const projEl = getEl('project-overview');
      if (projEl) projEl.hidden = true;
      const titleEl = getEl('active-session-title');
      if (titleEl) titleEl.textContent = title;
      const rail = typeof globalThis !== 'undefined' ? globalThis.__reSearchSessionRail : null;
      const handled = rail
        ? await rail.upsertSession?.({ id, title, mode: 'solve', archived: false }, { select: true })
        : false;
      if (inFlightMount?.sessionId === id) {
        await inFlightMount.promise;
      } else if (!handled || mountedSessionId !== id || mountedMode !== 'solve') {
        await mountSolveStage(id, ++syncSeq);
      }
      return res;
    } finally {
      creatingSolve = false;
    }
  }

  async function openExploreCreated(res) {
    const id = res?.id || res?.session?.id || res?.session_id;
    if (!id) return null;
    modeBySession.set(id, 'explore');
    const title = String(res?.title || res?.session?.title || 'Explore');
    const titleEl = getEl('active-session-title');
    if (titleEl && (res?.title || res?.session?.title)) titleEl.textContent = title;
    const rail = typeof globalThis !== 'undefined' ? globalThis.__reSearchSessionRail : null;
    await rail?.upsertSession?.({ id, title, mode: 'explore', archived: false }, { select: false });
    return mountExploreStage(id, ++syncSeq, res?.idea || null);
  }

  return {
    syncFromRail, openNewSolveSession, openExploreCreated,
    getActiveMode: () => mountedMode, getActiveSessionId: () => mountedSessionId,
    getSolveView: () => activeSolveView, getSolveWorkspace: () => activeSolveWorkspace,
    getExploreController: () => activeExploreCtrl,
    getSolveArtifacts: () => activeSolveArtifacts,
    getSolveTabs: () => Array.from(solveTabs.values()),
  };
}

if (typeof globalThis !== 'undefined') {
  globalThis.__reSearchModeStages = createModeStagesController();
}
