//! Solve loop desktop endpoints & lifecycle orchestration (S12d/S12e):
//! - `POST /api/sessions/{id}/solve/continue` (background Solve leg continuation)
//! - Startup `reconcile_interrupted_solve_sessions(state)` + explicit `POST /api/sessions/{id}/solve/reconcile`

use crate::routes_session::{bad, internal, not_found, parse_json_body, ApiResult};
use crate::session_lifecycle::{is_turn_running, reserve_turn};
use crate::session_mode::session_mode;
use crate::session_view::SseEventSink;
use crate::solve_runtime::build_solve_router;
use crate::state::DesktopState;
use crate::turn_control::ApiTurnGuard;
use agent::{
    resume_solve_loop, run_solve_loop, CancelToken, SolveLoopConfig, SolveWorkerRuntime,
    DEFAULT_MAX_TOKENS, DEFAULT_SOLVE_BRIEF_BUDGET_CHARS, DEFAULT_SOLVE_CONTEXT_WINDOW_CHARS,
};
use ledger::{Event, Ledger};
use research_tools::paper_key::Mode;
use research_tools::run_budget::{count_model_calls, RunScope};
use research_tools::solve_exprs::CheckCanonicalizer;
use research_tools::solve_gates::{emit_solve_paused, PauseTrigger};
use research_tools::solve_model::{event_kinds, PauseReason, SolveRunStatus};
use research_tools::solve_state::SolveState;
use research_tools::validate_session_id;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

fn resolve_solve_solver_model(
    state: &DesktopState,
    subagent: Option<&crate::subagents::SubagentDef>,
    req: &Value,
    setup: Option<&research_tools::solve_model::SolveSetup>,
    meta: Option<&crate::state::SessionMeta>,
    ui: &crate::state::UiSettings,
) -> Result<(agent::ProviderKind, String, String), String> {
    let has_subagent_override = subagent
        .and_then(|sa| sa.provider.as_deref().zip(sa.model.as_deref()))
        .is_some_and(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty());
    let req_p = req
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if has_subagent_override || req_p.is_some() {
        return crate::agent_models::resolve_orchestrator_model(subagent, req, meta, ui);
    }
    if let Some(solver) = setup.map(|s| &s.roles.solver) {
        let p_raw = solver.provider.trim();
        if !p_raw.is_empty() {
            if crate::cli_turn::CliRouteKind::from_provider(&p_raw.to_ascii_lowercase()).is_some() {
                return Err("Solve continuation requires an in-core API or ChatGPT provider".into());
            }
            let pk = agent::ProviderKind::parse(p_raw)?;
            let m_raw = solver.model.trim();
            let requested = if m_raw.is_empty() { pk.default_model() } else { m_raw }.to_string();
            let resolved = pk.resolve_model(&requested);
            return Ok((pk, requested, resolved));
        }
    }
    let has_configured_orch = meta
        .and_then(|m| m.default_provider.as_deref())
        .is_some_and(|s| !s.trim().is_empty())
        || ui.agent_models.contains_key("orchestrator")
        || ui.default_provider.as_deref().is_some_and(|s| !s.trim().is_empty());
    if !has_configured_orch {
        if let Ok((pk, model)) =
            crate::agent_models::resolve_in_core_role_model(state, "orchestrator", Some(req), true)
        {
            return Ok((pk, model.clone(), model));
        }
    }
    crate::agent_models::resolve_orchestrator_model(subagent, req, meta, ui)
}

pub(crate) fn run_desktop_solve_turn(
    state: &Arc<DesktopState>,
    session_id: &str,
    req: &Value,
    prompt: &str,
    cancel: &CancelToken,
) -> Result<String, String> {
    if cancel.is_cancelled() {
        return Err("stopped by user".into());
    }
    let ledger = state.open_ledger()?;
    let existing = ledger.read_session(session_id).map_err(|e| e.to_string())?;
    let solve_state = SolveState::from_events(&existing);
    let meta = state.load_meta(session_id);
    let ui = state.load_ui_settings();
    let (reasoning_effort, subagent) =
        crate::subagents::resolve_turn_options(state, session_id, req, meta.clone());
    let (provider, model_requested, model) = resolve_solve_solver_model(
        state,
        subagent.as_ref(),
        req,
        solve_state.setup.as_ref(),
        meta.as_ref(),
        &ui,
    )?;
    let max_tokens = req
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let env = Arc::new(state.research_env_for_session(session_id)?);
    let solver_fallback = research_tools::solve_model::RoleModel {
        provider: provider.cli_name().to_string(),
        model: model.clone(),
    };
    let transport = state.build_transport(provider, &model)?;
    let bundle = build_solve_router(Arc::clone(state), Arc::clone(&env), session_id)?;
    let mut sink = SseEventSink::new(Arc::clone(&state.sse), session_id).with_model_title(false);
    let hooks = crate::solve_host_hooks::DesktopSolveHostHooks::new(
        Arc::clone(state),
        Arc::clone(&env),
        req.clone(),
        solver_fallback,
    );
    let cfg = SolveLoopConfig {
        model_requested,
        model,
        max_tokens,
        reasoning_effort,
        containment: bundle.containment,
        contained: bundle.contained,
        prompt: prompt.to_string(),
        run_id: None,
        context_window_chars: DEFAULT_SOLVE_CONTEXT_WINDOW_CHARS,
        brief_budget_chars: DEFAULT_SOLVE_BRIEF_BUDGET_CHARS,
        extra_prompt_sections: Vec::new(),
        env,
        solve_gate: Some(bundle.gate),
        canonicalizer: bundle
            .check_bridge
            .as_ref()
            .map(|b| b.as_ref() as &dyn CheckCanonicalizer),
        worker_runtime: bundle
            .worker_runtime
            .as_ref()
            .map(|r| r.as_ref() as &dyn SolveWorkerRuntime),
        hooks: Some(&hooks),
        human: Some(&crate::hitl::DesktopHumanChannel::new(Arc::clone(state), session_id)),
        cancel: Some(cancel),
    };

    let has_started = existing.iter().any(|e| e.kind == "session.start");
    let outcome = if !has_started {
        run_solve_loop(&*transport, &bundle.router, &ledger, &cfg, &mut sink)?
    } else {
        resume_solve_loop(&*transport, &bundle.router, &ledger, &cfg, &mut sink)?
    };

    match outcome {
        agent::SessionOutcome::Aborted { reason, .. } => Err(reason),
        agent::SessionOutcome::Completed { .. }
        | agent::SessionOutcome::Refusal { .. }
        | agent::SessionOutcome::Paused { .. } => Ok(sink.final_text().to_string()),
    }
}

/// `POST /api/sessions/{id}/solve/continue`: resume a paused or open-items Solve session in background.
pub fn handle_continue(state: &Arc<DesktopState>, session_id: &str, body: &[u8]) -> ApiResult {
    validate_session_id(session_id).map_err(|e| bad(e.to_string()))?;
    if !state.session_exists(session_id).map_err(internal)? {
        return Err(not_found(format!("session `{session_id}` not found")));
    }
    let req: Value = if body.iter().all(|b| b.is_ascii_whitespace()) {
        json!({})
    } else {
        parse_json_body(body)?
    };
    let req_obj = req.as_object().ok_or_else(|| bad("request body must be a JSON object"))?;

    let ledger = state.open_ledger().map_err(internal)?;
    let events = ledger.read_session(session_id).map_err(|e| internal(e.to_string()))?;
    if session_mode(&events) != Mode::Solve {
        return Err(bad("`solve/continue` is only available for Solve sessions"));
    }
    let solve_state = SolveState::from_events(&events);
    if solve_state.setup.is_none() {
        return Err(bad("Solve session requires setup before continuing"));
    }

    let note = req_obj.get("note").and_then(Value::as_str).map(str::trim).unwrap_or("");
    let correct_entry_id = req_obj
        .get("correct_entry_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(ref cid) = correct_entry_id {
        if solve_state.entry(cid).is_none() {
            return Err(bad(format!("unknown proof step `{cid}` in `correct_entry_id`")));
        }
    }

    let meta = state.load_meta(session_id);
    let ui = state.load_ui_settings();
    if let Some(p) = req_obj.get("provider").and_then(Value::as_str).map(str::trim) {
        if matches!(p, "claude-cli" | "antigravity") {
            return Err(bad("Solve continuation requires an in-core API or ChatGPT provider"));
        }
    }
    if meta.as_ref().and_then(|m| m.route.as_deref()).is_some_and(|r| matches!(r, "claude-cli" | "antigravity")) {
        return Err(bad("Solve continuation requires an in-core API or ChatGPT provider"));
    }
    let _ = resolve_solve_solver_model(
        state,
        None,
        &req,
        solve_state.setup.as_ref(),
        meta.as_ref(),
        &ui,
    )
    .map_err(bad)?;

    let activity = reserve_turn(state, session_id)?;
    {
        let mut active = state.active_turns.lock().map_err(|_| internal("active_turns lock poisoned"))?;
        if !active.insert(session_id.to_string()) {
            return Err((409, format!("a turn is already running for `{session_id}`")));
        }
    }

    let prompt = match (&correct_entry_id, note.is_empty()) {
        (Some(cid), false) => format!("Correction for step `{cid}`: {note}"),
        (Some(cid), true) => format!("Correct step `{cid}` and re-check closure."),
        (None, false) => note.to_string(),
        (None, true) => String::new(),
    };
    if !prompt.is_empty() {
        let mut note_payload = json!({ "text": prompt, "source": "solve_continue" });
        if let Some(cid) = &correct_entry_id {
            note_payload["correct_entry_id"] = json!(cid);
        }
        let _ = ledger.append_with_hash(session_id, event_kinds::NOTE, 1, &note_payload);
    }

    let guard = match ApiTurnGuard::new(state, session_id) {
        Ok(g) => g,
        Err(e) => {
            if let Ok(mut active) = state.active_turns.lock() {
                active.remove(session_id);
            }
            return Err(internal(e));
        }
    };
    let st_thread = Arc::clone(state);
    let sid_thread = session_id.to_string();
    std::thread::Builder::new()
        .name(format!("solve-{session_id}"))
        .spawn(move || {
            let _activity = activity;
            let res = run_desktop_solve_turn(&st_thread, &sid_thread, &req, &prompt, &guard.cancel);
            drop(guard);
            if let Ok(mut active) = st_thread.active_turns.lock() {
                active.remove(&sid_thread);
            }
            match res {
                Ok(text) => {
                    st_thread.sse.emit(&sid_thread, "turn_complete", json!({ "session": sid_thread, "text": text }));
                }
                Err(err) => {
                    st_thread.sse.emit(&sid_thread, "turn_error", json!({ "session": sid_thread, "error": err }));
                }
            }
        })
        .map_err(|e| {
            if let Ok(mut active) = state.active_turns.lock() {
                active.remove(session_id);
            }
            internal(format!("failed to spawn solve thread: {e}"))
        })?;

    Ok((202, json!({ "ok": true, "accepted": true, "session": session_id })))
}

/// Explicit `POST /api/sessions/{id}/solve/reconcile` for an interrupted Solve leg (`THREAT-MODEL.md:333`).
pub fn handle_reconcile(state: &DesktopState, session_id: &str, _body: &[u8]) -> ApiResult {
    validate_session_id(session_id).map_err(|e| bad(e.to_string()))?;
    if !state.session_exists(session_id).map_err(internal)? {
        return Err(not_found(format!("session `{session_id}` not found")));
    }
    if is_turn_running(state, session_id) {
        return Err((409, format!("a turn is already running for `{session_id}`")));
    }
    let ledger = state.open_ledger().map_err(internal)?;
    let events = ledger.read_session(session_id).map_err(|e| internal(e.to_string()))?;
    if session_mode(&events) != Mode::Solve {
        return Err(bad("`solve/reconcile` is only available for Solve sessions"));
    }
    let reconciled = reconcile_one_session(&ledger, session_id, &events).map_err(internal)?;
    Ok((200, json!({ "ok": true, "session": session_id, "reconciled": reconciled })))
}

/// Startup reconciliation called once when `start_desktop_server` boots (never on `GET solve/view`).
pub fn reconcile_interrupted_solve_sessions(state: &DesktopState) -> usize {
    if !state.cfg.data_dir.join("ledger.db").is_file() {
        return 0;
    }
    let Ok(ro_ledger) = Ledger::open_existing(&state.cfg.data_dir) else {
        return 0;
    };
    let mut ids = BTreeSet::new();
    if let Ok(rows) = ro_ledger.sessions() {
        for (id, _) in rows {
            if validate_session_id(&id).is_ok() {
                ids.insert(id);
            }
        }
    }
    for id in state.list_meta_ids() {
        ids.insert(id);
    }

    let mut rw_ledger: Option<Ledger> = None;
    let mut count = 0usize;
    for sid in ids {
        if is_turn_running(state, &sid) {
            continue;
        }
        let Ok(events) = ro_ledger.read_session(&sid) else {
            continue;
        };
        if session_mode(&events) != Mode::Solve {
            continue;
        }
        let ledger_ref = match rw_ledger {
            Some(ref l) => l,
            None => match state.open_ledger() {
                Ok(l) => rw_ledger.insert(l),
                Err(_) => return count,
            },
        };
        if reconcile_one_session(ledger_ref, &sid, &events).unwrap_or(false) {
            count += 1;
        }
    }
    count
}

fn reconcile_one_session(ledger: &Ledger, session_id: &str, events: &[Event]) -> Result<bool, String> {
    let state = SolveState::from_events(events);
    if state.run_status != SolveRunStatus::Running {
        return Ok(false);
    }
    let Some(last_leg_start) = events
        .iter()
        .rposition(|e| matches!(e.kind.as_str(), "session.start" | "session.resume"))
    else {
        return Ok(false);
    };
    let tail = &events[last_leg_start..];
    if tail.iter().any(|e| {
        matches!(
            e.kind.as_str(),
            "solve.paused" | "solve.done" | "session.end" | "session.abort"
        )
    }) {
        return Ok(false);
    }
    let leg = tail
        .iter()
        .rev()
        .find_map(|e| e.payload.get("leg").and_then(Value::as_u64))
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);
    let turn = tail
        .iter()
        .rev()
        .find(|e| e.kind == "model.request")
        .and_then(|e| e.payload.get("turn").and_then(Value::as_u64))
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let used = u32::try_from(count_model_calls(events, RunScope::All)).unwrap_or(u32::MAX);
    emit_solve_paused(
        ledger,
        session_id,
        &PauseTrigger {
            reason: PauseReason::Interrupted,
            message: "Solve leg interrupted before completion".into(),
            open_items: Vec::new(),
        },
        leg,
        turn,
        used,
    )?;
    Ok(true)
}
