// Centre-stage Solve setup card & first screen (package S5b; `solve.md` 2, 3, D2, D4, D5, D15, D20, D21;
// `00-overview.md` 5.1, 8.3 #9). Zero modal dialogs; works with zero model connected; zero `innerHTML` writes.

import { createApprovalCard } from './approval_card.js';
import { katexOptions } from './math_katex.js';
import { extractMath } from './math_scan.js';
import {
  ASK_BEFORE_RUNNING_LABEL,
  NO_MODEL_BANNER,
  PROVE_PROSE_NOTICE,
  SAME_MODEL_WARNING,
  SYMBOL_ASSUMPTION_PRESETS,
  TASK_KINDS,
  TIER_NUMERIC_WORD,
  TIER_VERIFIED_WORD,
  applyPrefillToState,
  applySolveViewToState,
  buildLocalGivenPreviewSteps,
  buildSetupSubmission,
  createInitialSolveState,
  defaultCriteriaForKind,
  defaultTargetForKind,
  formatCriterionLabel,
  isSameModelSelection,
  summarizeCollapsedSetup,
} from './solve_state.js';
import { createBadge } from './ui_badge.js';

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function textInput(cls, placeholder, value, onInput) {
  const inp = el('input', cls);
  inp.type = 'text';
  inp.placeholder = placeholder;
  inp.value = value;
  inp.addEventListener('input', (ev) => onInput(ev.target.value));
  return inp;
}

function checkboxRow(labelText, checked, testId, onChange) {
  const row = el('label', 'solve-check-row');
  const box = el('input');
  box.type = 'checkbox';
  box.checked = Boolean(checked);
  if (testId) box.setAttribute('data-testid', testId);
  box.addEventListener('change', (ev) => onChange(Boolean(ev.target.checked)));
  row.append(box, el('span', '', labelText));
  return row;
}

// Live statement math preview with zero innerHTML (uses katex.render + textContent only).
export function renderSafeMathPreview(container, rawText, katexImpl = globalThis.window?.katex) {
  if (!container) return;
  const text = String(rawText ?? '');
  container.replaceChildren();
  if (!text.trim()) {
    container.append(el('span', 'solve-preview-empty', 'Live math preview appears here as you type.'));
    return;
  }
  const tokens = [];
  const marked = extractMath(text.replace(/\u0000/g, ''), tokens, {
    protectInlineCode: true,
    allowSingleDollar: true,
  });
  let last = 0;
  for (const m of marked.matchAll(/\u0000M(\d+)\u0000/g)) {
    const plain = marked.slice(last, m.index);
    if (plain) container.append(el('span', 'solve-preview-text', plain));
    const tok = tokens[Number(m[1])];
    if (tok) {
      const mathEl = el('span', tok.display ? 'solve-preview-math is-display' : 'solve-preview-math');
      if (katexImpl && typeof katexImpl.render === 'function') {
        try {
          katexImpl.render(tok.tex, mathEl, katexOptions(tok.display));
        } catch {
          mathEl.textContent = tok.tex;
        }
      } else {
        mathEl.textContent = tok.tex;
      }
      container.append(mathEl);
    }
    last = m.index + m[0].length;
  }
  const tail = marked.slice(last);
  if (tail) container.append(el('span', 'solve-preview-text', tail));
}

function buildModelOptions(selectEl, models, current) {
  selectEl.replaceChildren();
  const noneOpt = el('option', '', 'Not selected');
  noneOpt.value = '';
  selectEl.append(noneOpt);
  const currentKey = current?.provider && current?.model ? `${current.provider}::${current.model}` : '';
  let seenCurrent = false;
  for (const item of Array.isArray(models) ? models : []) {
    const key = `${item.provider}::${item.model}`;
    if (key === currentKey) seenCurrent = true;
    const opt = el('option', '', item.label || `${item.provider} / ${item.model}`);
    opt.value = key;
    if (key === currentKey) opt.selected = true;
    selectEl.append(opt);
  }
  if (currentKey && !seenCurrent) {
    const opt = el('option', '', `${current.provider} / ${current.model}`);
    opt.value = currentKey;
    opt.selected = true;
    selectEl.append(opt);
  }
  selectEl.value = currentKey;
}

function parseModelKey(val) {
  const s = String(val || '');
  const idx = s.indexOf('::');
  if (idx < 0) return { provider: '', model: '' };
  return { provider: s.slice(0, idx), model: s.slice(idx + 2) };
}

export function createSolveSetupView(host, options = {}) {
  if (!host) throw new Error('createSolveSetupView requires a host element');

  let state = createInitialSolveState(options.initial || {});
  const onPrefill = typeof options.onPrefill === 'function' ? options.onPrefill : null;
  const onSubmitSetup = typeof options.onSubmitSetup === 'function' ? options.onSubmitSetup : null;
  const onAllowGrant = typeof options.onAllowGrant === 'function' ? options.onAllowGrant : null;
  const onDenyGrant = typeof options.onDenyGrant === 'function' ? options.onDenyGrant : null;
  const onRecheck = typeof options.onRecheck === 'function' ? options.onRecheck : null;
  const katexImpl = options.katex ?? globalThis.window?.katex ?? null;

  let approvalCardApi = null;
  let prefillTimer = null;
  let prefillSeq = 0;
  const root = el('section', 'solve-stage');
  root.setAttribute('data-component', 'solve-setup-stage');

  function updateSameModelFlags() {
    state.sameModel = isSameModelSelection(state.solver, state.verifier);
    state.sameModelWarning = state.sameModel ? SAME_MODEL_WARNING : null;
  }

  async function triggerPrefill() {
    if (!onPrefill) return;
    const seq = ++prefillSeq;
    try {
      const resp = await onPrefill({
        statement: state.statement,
        conventions: state.conventions,
        task_kind: null,
        solver: state.solver,
        verifier: state.verifier,
        allow_numeric_target: state.allowNumericTarget,
      });
      if (seq !== prefillSeq) return;
      if (resp) {
        state = applyPrefillToState(state, resp);
        render();
      }
    } catch (err) {
      if (seq !== prefillSeq) return;
      state.error = String(err?.message || err || 'Prefill failed');
      render();
    }
  }

  function schedulePrefill() {
    if (!onPrefill) return;
    if (prefillTimer) clearTimeout(prefillTimer);
    prefillTimer = setTimeout(() => { triggerPrefill(); }, options.debounceMs ?? 250);
  }

  async function submitSetup() {
    if (prefillTimer) { clearTimeout(prefillTimer); prefillTimer = null; }
    prefillSeq += 1;
    const payload = buildSetupSubmission(state);
    if (!payload.statement) {
      state.error = 'Enter a problem statement before running.';
      render();
      return null;
    }
    state.error = '';
    if (!onSubmitSetup) {
      state = applySolveViewToState(state, {
        setup: { ...payload, revision: (state.setup?.revision || 0) + 1 },
        steps: buildLocalGivenPreviewSteps(payload),
      });
      state.phase = 'collapsed';
      render();
      return payload;
    }
    try {
      const res = await onSubmitSetup(payload);
      const view = res?.view || {
        setup: res?.setup || { ...payload, revision: (state.setup?.revision || 0) + 1 },
        steps: res?.steps || buildLocalGivenPreviewSteps(payload),
      };
      state.phase = 'collapsed';
      state = applySolveViewToState(state, view);
      render();
      return res;
    } catch (err) {
      state.error = String(err?.message || err || 'Setup submission failed');
      render();
      return null;
    }
  }

  function renderCollapsed() {
    const bar = el('div', 'solve-collapsed-bar');
    bar.setAttribute('data-testid', 'solve-collapsed-bar');
    bar.append(el('span', 'solve-collapsed-summary', summarizeCollapsedSetup(state.setup)));

    const btns = el('div', 'solve-collapsed-actions');
    const editBtn = el('button', 'pill-btn sm', 'Edit setup');
    editBtn.type = 'button';
    editBtn.setAttribute('data-action', 'edit-setup');
    editBtn.addEventListener('click', () => {
      state.phase = 'revising';
      render();
    });
    btns.append(editBtn);

    const recheckBtn = el('button', 'pill-btn sm', 'Re-run checks');
    recheckBtn.type = 'button';
    recheckBtn.setAttribute('data-action', 'recheck');
    recheckBtn.addEventListener('click', async () => {
      if (onRecheck) {
        await onRecheck(state.setup);
      } else {
        state.recheckNotice = 'Re-running checks works without a model once checks exist (S9).';
        render();
      }
    });
    btns.append(recheckBtn);
    bar.append(btns);
    root.append(bar);

    if (state.recheckNotice) {
      root.append(el('div', 'solve-info-banner', state.recheckNotice));
    }

    const givenWrap = el('div', 'solve-given-list');
    givenWrap.setAttribute('data-testid', 'solve-given-steps');
    const steps = state.steps.filter((s) => !s.superseded_by);
    const allGiven = steps.length === 0 || steps.every((s) => (s.badge?.label || 'Given') === 'Given');
    givenWrap.append(el('h3', 'solve-section-title', allGiven ? 'Given steps' : 'Steps'));
    if (steps.length === 0) {
      givenWrap.append(el('p', 'solve-muted', 'No setup conventions or assumptions recorded.'));
    } else {
      for (const step of steps) {
        const card = el('article', 'solve-given-card');
        card.setAttribute('data-entry-id', String(step.entry_id || ''));
        const head = el('div', 'solve-given-head');
        head.append(el('span', 'solve-step-id', String(step.entry_id || '')));
        head.append(el('strong', 'solve-step-summary', String(step.summary || '')));
        const badgeLabel = step.badge?.label || 'Given';
        head.append(createBadge({
          text: badgeLabel,
          icon: step.badge?.icon || 'user',
          tone: step.badge?.tone || 'neutral',
        }));
        if (step.badge?.provenance_tag) {
          head.append(el('span', 'mini-pill solve-tag-you', String(step.badge.provenance_tag)));
        }
        card.append(head);
        if (step.details) {
          card.append(el('p', 'solve-step-details', String(step.details)));
        }
        givenWrap.append(card);
      }
    }
    root.append(givenWrap);
  }

  function renderSymbolsSection(card) {
    const sec = el('div', 'solve-field');
    const head = el('div', 'solve-field-head');
    head.append(el('label', 'solve-label', 'Symbols and assumptions'));
    const addBtn = el('button', 'pill-btn sm', '+ Add symbol');
    addBtn.type = 'button';
    addBtn.setAttribute('data-action', 'add-symbol');
    addBtn.addEventListener('click', () => {
      state.symbols.push({ name: '', assumptions: ['real'] });
      render();
    });
    head.append(addBtn);
    sec.append(head);

    const table = el('div', 'solve-table');
    table.setAttribute('data-testid', 'solve-symbols-table');
    if (state.symbols.length === 0) {
      table.append(el('div', 'solve-muted', 'Detected symbols from your statement appear here.'));
    }
    state.symbols.forEach((sym, idx) => {
      const row = el('div', 'solve-table-row');
      const nameIn = el('input', 'text-input solve-sym-name');
      nameIn.type = 'text';
      nameIn.value = sym.name;
      nameIn.placeholder = 'symbol';
      nameIn.setAttribute('aria-label', `Symbol ${idx + 1} name`);
      nameIn.addEventListener('input', (ev) => { sym.name = ev.target.value; });

      const assumpSel = el('select', 'compact-select solve-sym-assump');
      assumpSel.setAttribute('aria-label', `Symbol ${idx + 1} assumptions`);
      const curText = sym.assumptions.join(', ') || 'real';
      const presets = SYMBOL_ASSUMPTION_PRESETS.includes(curText)
        ? SYMBOL_ASSUMPTION_PRESETS
        : [curText, ...SYMBOL_ASSUMPTION_PRESETS];
      for (const preset of presets) {
        const opt = el('option', '', preset);
        opt.value = preset;
        if (preset === curText) opt.selected = true;
        assumpSel.append(opt);
      }
      assumpSel.value = curText;
      assumpSel.addEventListener('change', (ev) => {
        sym.assumptions = String(ev.target.value || '').split(',').map((x) => x.trim()).filter(Boolean);
      });

      const delBtn = el('button', 'pill-btn sm', 'Remove');
      delBtn.type = 'button';
      delBtn.addEventListener('click', () => {
        state.symbols.splice(idx, 1);
        render();
      });
      row.append(nameIn, assumpSel, delBtn);
      table.append(row);
    });
    sec.append(table);
    card.append(sec);
  }

  function renderDimensionsSection(card) {
    const sec = el('div', 'solve-field');
    const head = el('div', 'solve-field-head');
    head.append(el('label', 'solve-label', 'Dimensions (optional)'));
    const addBtn = el('button', 'pill-btn sm', '+ Add dimension');
    addBtn.type = 'button';
    addBtn.setAttribute('data-action', 'add-dimension');
    addBtn.addEventListener('click', () => {
      state.dimensions.push({ symbol: '', base: 'mass', power: '1' });
      render();
    });
    head.append(addBtn);
    sec.append(head);

    const table = el('div', 'solve-table');
    table.setAttribute('data-testid', 'solve-dimensions-table');
    state.dimensions.forEach((dim, idx) => {
      const row = el('div', 'solve-table-row');
      const symIn = textInput('text-input', 'symbol', dim.symbol, (v) => { dim.symbol = v; });
      const baseIn = textInput('text-input', 'base (e.g. mass)', dim.base, (v) => { dim.base = v; });
      const powIn = textInput('text-input', 'power (e.g. 1)', dim.power, (v) => { dim.power = v; });
      const delBtn = el('button', 'pill-btn sm', 'Remove');
      delBtn.type = 'button';
      delBtn.addEventListener('click', () => { state.dimensions.splice(idx, 1); render(); });
      row.append(symIn, baseIn, powIn, delBtn);
      table.append(row);
    });
    sec.append(table);
    card.append(sec);
  }

  function renderChecklistSection(card) {
    const sec = el('div', 'solve-field');
    sec.append(el('label', 'solve-label', 'Done checklist'));

    if (state.taskKind !== 'prove') {
      const tierRow = el('div', 'solve-tier-toggle');
      tierRow.append(
        checkboxRow(
          `Allow ${TIER_NUMERIC_WORD} (instead of requiring exact ${TIER_VERIFIED_WORD})`,
          state.allowNumericTarget,
          'solve-allow-numeric-toggle',
          (checked) => {
            state.allowNumericTarget = checked;
            const targetTier = checked ? 'checked_numerically' : 'verified';
            for (const item of state.done) {
              if (item.id === 'final_result_tier' || item.id === 'closure_checked') {
                item.params = { ...(item.params || {}), tier: targetTier };
              }
            }
            render();
          },
        ),
        checkboxRow(`Every step ${TIER_VERIFIED_WORD}`, state.everyStepVerified, 'solve-every-step-verified', (c) => {
          state.everyStepVerified = c;
        }),
      );
      sec.append(tierRow);
    }

    const list = el('div', 'solve-checklist');
    list.setAttribute('data-testid', 'solve-done-checklist');
    state.done.forEach((crit) => {
      list.append(checkboxRow(formatCriterionLabel(crit), crit.enabled !== false, null, (c) => { crit.enabled = c; }));
    });
    sec.append(list);
    card.append(sec);
  }

  function renderSetupCard() {
    const card = el('div', 'solve-setup-card');
    card.setAttribute('data-testid', 'solve-setup-card');

    const head = el('div', 'solve-setup-head');
    head.append(el('h2', 'solve-setup-title', state.phase === 'revising' ? 'Revise Solve setup' : 'Solve a problem'));
    const toolboxLine = el('span', 'solve-toolbox-line', state.toolbox?.line || 'Toolbox: unavailable');
    toolboxLine.setAttribute('data-testid', 'solve-toolbox-line');
    head.append(toolboxLine);
    card.append(head);

    // 3-way kind switch: Calculate / Prove / Check a derivation.
    const kindBar = el('div', 'solve-kind-switch');
    kindBar.setAttribute('role', 'radiogroup');
    kindBar.setAttribute('aria-label', 'Solve task kind');
    for (const kind of TASK_KINDS) {
      const btn = el('button', state.taskKind === kind.id ? 'seg-btn is-active' : 'seg-btn', kind.label);
      btn.type = 'button';
      btn.setAttribute('data-kind', kind.id);
      btn.setAttribute('aria-pressed', state.taskKind === kind.id ? 'true' : 'false');
      btn.addEventListener('click', () => {
        if (state.taskKind === kind.id) return;
        state.taskKind = kind.id;
        state.target = defaultTargetForKind(kind.id);
        state.proveNotice = kind.id === 'prove' ? PROVE_PROSE_NOTICE : null;
        state.done = defaultCriteriaForKind(kind.id, state.allowNumericTarget);
        render();
      });
      kindBar.append(btn);
    }
    card.append(kindBar);

    if (state.taskKind === 'prove') {
      const notice = el('div', 'solve-prove-notice', state.proveNotice || PROVE_PROSE_NOTICE);
      notice.setAttribute('data-testid', 'solve-prove-notice');
      card.append(notice);
    }

    // Statement input + live KaTeX preview.
    const stmtField = el('div', 'solve-field');
    stmtField.append(el('label', 'solve-label', 'Problem statement (TeX supported, max 16,000 chars)'));
    const stmtIn = el('textarea', 'text-input solve-statement-input');
    stmtIn.rows = 4;
    stmtIn.value = state.statement;
    stmtIn.placeholder = 'State the calculation, proof, or derivation to check…';
    stmtIn.setAttribute('maxlength', '16000');
    stmtIn.setAttribute('data-testid', 'solve-statement-input');
    const preview = el('div', 'solve-katex-preview');
    preview.setAttribute('data-testid', 'solve-katex-preview');
    renderSafeMathPreview(preview, state.statement, katexImpl);
    stmtIn.addEventListener('input', (ev) => {
      state.statement = ev.target.value;
      renderSafeMathPreview(preview, state.statement, katexImpl);
      schedulePrefill();
    });
    stmtField.append(stmtIn, preview);
    card.append(stmtField);

    renderSymbolsSection(card);

    // Conventions textarea (max 2,000 chars).
    const convField = el('div', 'solve-field');
    convField.append(el('label', 'solve-label', 'Conventions (max 2,000 chars)'));
    const convIn = el('textarea', 'text-input solve-conventions-input');
    convIn.rows = 2;
    convIn.value = state.conventions;
    convIn.placeholder = 'Euclidean signature, hbar = c = 1, MS-bar…';
    convIn.setAttribute('maxlength', '2000');
    convIn.setAttribute('data-testid', 'solve-conventions-input');
    convIn.addEventListener('input', (ev) => { state.conventions = ev.target.value; });
    convField.append(convIn);
    card.append(convField);

    renderDimensionsSection(card);
    renderChecklistSection(card);

    // Solver / Verifier model selectors + D2 same-model warning.
    const modelSec = el('div', 'solve-models-grid');
    const modelField = (label, testId, current, assign) => {
      const wrap = el('div', 'solve-field');
      wrap.append(el('label', 'solve-label', label));
      const sel = el('select', 'compact-select');
      sel.setAttribute('data-testid', testId);
      buildModelOptions(sel, state.connectedModels, current);
      sel.addEventListener('change', (ev) => {
        assign(parseModelKey(ev.target.value));
        updateSameModelFlags();
        render();
      });
      wrap.append(sel);
      return wrap;
    };
    modelSec.append(
      modelField('Solver model', 'solve-solver-select', state.solver, (v) => { state.solver = v; }),
      modelField('Verifier model', 'solve-verifier-select', state.verifier, (v) => { state.verifier = v; }),
    );
    card.append(modelSec);

    if (state.sameModel) {
      const warn = el('div', 'solve-warning-banner');
      warn.setAttribute('data-testid', 'solve-same-model-warning');
      warn.append(
        el('span', '', state.sameModelWarning || SAME_MODEL_WARNING),
        checkboxRow('Accept same-model review', state.acceptSameModel, 'solve-accept-same-model', (c) => {
          state.acceptSameModel = c;
        }),
      );
      card.append(warn);
    }

    // Shared approval card (S5a, D5).
    const approvalHost = el('div', 'solve-approval-host');
    approvalHost.setAttribute('data-testid', 'solve-approval-host');
    approvalCardApi = createApprovalCard(approvalHost, {
      title: 'Approvals for this Solve run',
      grants: state.approvalGrants?.grants || [],
      preTurnSupported: state.approvalGrants?.pre_turn_approver_supported !== false,
      onAllowGrant,
      onDenyGrant,
      onChange: (nextGrants) => {
        state.approvalGrants = { ...(state.approvalGrants || {}), grants: nextGrants };
      },
    });
    card.append(approvalHost);

    // D20 opt-in switch + Run button.
    const footer = el('div', 'solve-setup-footer');
    footer.append(checkboxRow(ASK_BEFORE_RUNNING_LABEL, state.askBeforeRunning, 'solve-ask-before-running', (c) => {
      state.askBeforeRunning = c;
    }));

    const actions = el('div', 'solve-setup-actions');
    if (state.phase === 'revising') {
      const cancelBtn = el('button', 'pill-btn', 'Cancel');
      cancelBtn.type = 'button';
      cancelBtn.setAttribute('data-action', 'cancel-revision');
      cancelBtn.addEventListener('click', () => {
        state.phase = 'collapsed';
        render();
      });
      actions.append(cancelBtn);
    }
    const runBtn = el('button', 'pill-btn primary solve-run-btn', state.phase === 'revising' ? 'Save revision' : 'Run');
    runBtn.type = 'button';
    runBtn.setAttribute('data-testid', 'solve-run-btn');
    runBtn.addEventListener('click', () => { submitSetup(); });
    actions.append(runBtn);
    footer.append(actions);
    card.append(footer);

    if (state.error) {
      const err = el('div', 'solve-error-banner', state.error);
      err.setAttribute('data-testid', 'solve-error-banner');
      card.append(err);
    }

    root.append(card);
  }

  function render() {
    root.replaceChildren();
    if (!state.modelConnected) {
      const banner = el('div', 'solve-no-model-banner', NO_MODEL_BANNER);
      banner.setAttribute('data-testid', 'solve-no-model-banner');
      root.append(banner);
    }
    if (state.phase === 'collapsed' && state.setup) {
      renderCollapsed();
    } else {
      renderSetupCard();
    }
  }

  host.replaceChildren(root);
  render();

  return {
    element: root,
    getState: () => ({ ...state }),
    getApprovalCard: () => approvalCardApi,
    applyPrefill(resp) {
      state = applyPrefillToState(state, resp);
      render();
    },
    applyView(view) {
      state = applySolveViewToState(state, view);
      render();
    },
    submitSetup,
    triggerPrefill,
  };
}
