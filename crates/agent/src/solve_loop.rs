//! In-core Solve execution loop (`solve/1`, `solve.md` 5.2–5.5, rules R12 & R16; S12d).
//!
//! Never emits `session.abort` (R16): every limit, cancellation, worker restart, or
//! transport failure emits `solve.paused` v1 and returns [`SessionOutcome::Paused`].

#[path = "solve_loop_tools.rs"]
mod loop_tools;

use crate::dispatch::collect_text;
use crate::interaction::{ask_user_tool_spec, inject_pending_steers, HumanChannel};
use crate::resume_brief::build_resume_brief;
use crate::runner::{
    record_content_blocks, tool_defs_json, validate_turn, Audit, AuditStreamObserver, EventSink,
    SessionOutcome,
};
use crate::solve_prompts::build_solve_system_prompt;
use crate::transport::{
    CancelToken, ContentBlock, Message, ModelRequest, Role, StopReason, ToolSpec, Transport,
    TransportErrorKind,
};
use ledger::Ledger;
use loop_tools::{
    dispatch_solve_tools, emit_brief_resume, estimate_prompt_chars, latest_leg, latest_run_id,
    latest_turn, pause_on_transport, pause_solve, validate_solve_cfg, write_checkpoint,
    DispatchStep,
};
use research_tools::lessons_prompt::{
    extract_solve_match_context, prepare_mode_lessons, LESSONS_SUPPLIED_EVENT,
};
use research_tools::solve_exprs::CheckCanonicalizer;
use research_tools::solve_gates::{PauseTrigger, SolveGate};
use research_tools::solve_model::{HandleRef, PauseReason, SolveExpr, SolveSetup};
use research_tools::solve_state::SolveState;
use research_tools::tool_profile::PROFILE_SOLVE_V1;
use research_tools::ResearchEnv;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tools::canon::random_hex;
use tools::Router;

pub const SOLVE_CALLER: &str = "solver";
pub const SOLVE_ROUTER_CALLER: &str = "solver:solve";
pub const DEFAULT_SOLVE_CONTEXT_WINDOW_CHARS: usize = 400_000;
pub const DEFAULT_SOLVE_BRIEF_BUDGET_CHARS: usize = 32_000;
const AUTO_CHECKPOINT_PERCENT: usize = 60;

/// Clean host-handled extension seam for future Solve packages (`S14` `review_steps` &
/// finish verifier pass, `S20` `compute_blind`) without modifying the frozen `solve/1` registry.
pub trait SolveHostHooks: Send + Sync {
    fn extra_tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn handle_host_tool(
        &self,
        _ledger: &Ledger,
        _session: &str,
        _tool_name: &str,
        _call_id: &str,
        _input: &Value,
        _cancel: Option<&CancelToken>,
    ) -> Option<Result<(bool, String), String>> {
        None
    }

    fn before_solve_finish(
        &self,
        _ledger: &Ledger,
        _session: &str,
        _input: &Value,
        _cancel: Option<&CancelToken>,
    ) -> Result<Option<PauseTrigger>, String> {
        Ok(None)
    }

    fn after_solve_finish(
        &self,
        _ledger: &Ledger,
        _session: &str,
        _finish_result: &Value,
    ) -> Result<(), String> {
        Ok(())
    }
}

pub struct NoopSolveHostHooks;
impl SolveHostHooks for NoopSolveHostHooks {}

/// Optional worker lifecycle seam so `solve_loop` can restage `solve.expr` handles on restart.
pub trait SolveWorkerRuntime: Send + Sync {
    fn worker_epoch(&self) -> Option<u64>;

    fn restage_handles(
        &self,
        ledger: &Ledger,
        session: &str,
        handles: &BTreeMap<HandleRef, SolveExpr>,
    ) -> Result<(), String>;
}

pub struct SolveLoopConfig<'a> {
    pub model_requested: String,
    pub model: String,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
    pub containment: String,
    pub contained: bool,
    pub prompt: String,
    pub run_id: Option<String>,
    pub context_window_chars: usize,
    pub brief_budget_chars: usize,
    pub extra_prompt_sections: Vec<String>,
    pub env: Arc<ResearchEnv>,
    pub solve_gate: Option<Arc<SolveGate>>,
    pub canonicalizer: Option<&'a dyn CheckCanonicalizer>,
    pub worker_runtime: Option<&'a dyn SolveWorkerRuntime>,
    pub hooks: Option<&'a dyn SolveHostHooks>,
    pub human: Option<&'a dyn HumanChannel>,
    pub cancel: Option<&'a CancelToken>,
}

pub(super) struct LegContext {
    pub(super) leg: u32,
    pub(super) run_id: String,
    pub(super) start_turn: u32,
    pub(super) total_turns_before: u32,
    pub(super) messages: Vec<Message>,
    pub(super) repair_used: bool,
    pub(super) pending_repair_prompt: Option<String>,
    pub(super) supplied_lessons_section: Option<String>,
}

/// Start a fresh in-core Solve run (`tool_profile: "solve/1"`, `caller: "solver"`).
pub fn run_solve_loop(
    transport: &dyn Transport,
    router: &Router,
    ledger: &Ledger,
    cfg: &SolveLoopConfig<'_>,
    sink: &mut dyn EventSink,
) -> Result<SessionOutcome, String> {
    validate_solve_cfg(router, cfg)?;
    let session = router.session();
    let existing = ledger.read_session(session).map_err(|e| e.to_string())?;
    let state = SolveState::from_events(&existing);
    let setup = state
        .setup
        .clone()
        .ok_or_else(|| "Solve session requires `solve.setup` before starting a run".to_string())?;
    if existing.iter().any(|e| e.kind == "session.start") {
        return Err(format!("Solve session `{session}` has already started; use `resume_solve_loop`"));
    }

    let run_id = cfg.run_id.clone().unwrap_or_else(|| format!("solve-{}", random_hex(8)));
    let info = transport.info();
    let defs = router.registry().defs();
    let lesson_ctx = extract_solve_match_context(&setup, &existing);
    let prepared_lessons = prepare_mode_lessons(&cfg.env.data_dir, &lesson_ctx, &run_id)?;
    let mut prompt_extra: Vec<&str> = cfg.extra_prompt_sections.iter().map(String::as_str).collect();
    if let Some(p) = &prepared_lessons {
        prompt_extra.push(p.prompt_section.as_str());
    }
    let system_prompt = build_solve_system_prompt(&setup, &prompt_extra);
    let first_user_text = if cfg.prompt.trim().is_empty() {
        let stmt = setup.statement.trim();
        if stmt.is_empty() {
            "Solve the problem stated in the instructions.".to_string()
        } else {
            format!("Solve the problem stated in the instructions.\n\n## Problem\n{stmt}")
        }
    } else {
        cfg.prompt.clone()
    };
    let first_user_text_sha256 = ledger::sha256_hex(first_user_text.as_bytes());
    let mut audit = Audit {
        ledger,
        session,
        sink,
        last_seq: existing.last().map_or(0, |e| e.seq),
    };
    let start_ev = audit.emit(
        "session.start",
        json!({
            "route": "api",
            "provenance": "in-core loop, full provenance",
            "provider": info.provider,
            "endpoint": info.endpoint,
            "api_key_env": info.api_key_env,
            "model_requested": cfg.model_requested,
            "model": cfg.model,
            "prompt": cfg.prompt,
            "first_user_text_sha256": first_user_text_sha256,
            "system_prompt": system_prompt,
            "max_turns": setup.budgets.max_model_calls_per_leg,
            "max_tokens": cfg.max_tokens,
            "policy_version": router.policy().version.as_str(),
            "containment": cfg.containment,
            "contained": cfg.contained,
            "tool_profile": PROFILE_SOLVE_V1,
            "run_id": run_id,
            "caller": SOLVE_CALLER,
            "leg": 1,
            "tool_defs": tool_defs_json(&defs),
        }),
    )?;
    let supplied_lessons_section = if let Some(prepared) = prepared_lessons {
        let raw_blob = audit.put_blob(prepared.blob_text.as_bytes())?;
        let ev_payload = prepared.build_event_payload(&raw_blob);
        audit.emit(LESSONS_SUPPLIED_EVENT, ev_payload)?;
        prepared.commit_uses(&cfg.env.data_dir)?;
        Some(prepared.prompt_section)
    } else {
        None
    };

    let gate = match &cfg.solve_gate {
        Some(g) => {
            g.set_leg_start_seq(start_ev.seq);
            Arc::clone(g)
        }
        None => Arc::new(SolveGate::new(Arc::clone(&cfg.env), start_ev.seq, start_ev.seq)),
    };
    let initial_messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: first_user_text }],
    }];
    let ctx = LegContext {
        leg: 1,
        run_id,
        start_turn: 1,
        total_turns_before: 0,
        messages: initial_messages,
        repair_used: false,
        pending_repair_prompt: None,
        supplied_lessons_section,
    };
    run_solve_legs(transport, router, &mut audit, &setup, &gate, cfg, ctx)
}

/// Resume a Solve run strictly from a resume brief (`resume_mode: "brief"`, no response replay).
pub fn resume_solve_loop(
    transport: &dyn Transport,
    router: &Router,
    ledger: &Ledger,
    cfg: &SolveLoopConfig<'_>,
    sink: &mut dyn EventSink,
) -> Result<SessionOutcome, String> {
    validate_solve_cfg(router, cfg)?;
    let session = router.session();
    let existing = ledger.read_session(session).map_err(|e| e.to_string())?;
    let state = SolveState::from_events(&existing);
    let setup = state
        .setup
        .clone()
        .ok_or_else(|| "Solve session requires `solve.setup` before resuming".to_string())?;
    if !existing.iter().any(|e| e.kind == "session.start") {
        return run_solve_loop(transport, router, ledger, cfg, sink);
    }

    let mut audit = Audit {
        ledger,
        session,
        sink,
        last_seq: existing.last().map_or(0, |e| e.seq),
    };
    let prev_leg = latest_leg(&existing);
    let next_leg = prev_leg.saturating_add(1);
    let run_id = cfg
        .run_id
        .clone()
        .or_else(|| latest_run_id(&existing))
        .unwrap_or_else(|| format!("solve-{}", random_hex(8)));
    let run_start_seq = existing
        .iter()
        .find(|e| e.kind == "session.start")
        .map_or(0, |e| e.seq.saturating_sub(1));

    let (brief, brief_sha256) = match &state.last_checkpoint {
        Some(cp) if cp.upto_seq == audit.last_seq => {
            let loaded = ledger
                .get_blob(&cp.brief_blob)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_else(|| build_resume_brief(&existing, cp.upto_seq, cfg.brief_budget_chars));
            (loaded, cp.brief_sha256.clone())
        }
        _ => write_checkpoint(&mut audit, &existing, prev_leg, "continue", cfg.brief_budget_chars)?,
    };

    let resume_ev = emit_brief_resume(
        &mut audit,
        transport,
        router,
        cfg,
        &setup,
        next_leg,
        &run_id,
        &brief,
        &brief_sha256,
    )?;
    let gate = match &cfg.solve_gate {
        Some(g) => {
            g.set_leg_start_seq(resume_ev.seq);
            Arc::clone(g)
        }
        None => Arc::new(SolveGate::new(Arc::clone(&cfg.env), run_start_seq, resume_ev.seq)),
    };
    let next_turn = latest_turn(&existing).saturating_add(1);
    let user_msg = if cfg.prompt.trim().is_empty() {
        brief
    } else {
        format!("{brief}\n\n## User Continuation Note\n{}", cfg.prompt.trim())
    };
    let ctx = LegContext {
        leg: next_leg,
        run_id,
        start_turn: next_turn,
        total_turns_before: 0,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: user_msg }],
        }],
        repair_used: false,
        pending_repair_prompt: None,
        supplied_lessons_section: None,
    };
    run_solve_legs(transport, router, &mut audit, &setup, &gate, cfg, ctx)
}

fn run_solve_legs(
    transport: &dyn Transport,
    router: &Router,
    audit: &mut Audit<'_>,
    setup: &SolveSetup,
    gate: &Arc<SolveGate>,
    cfg: &SolveLoopConfig<'_>,
    mut ctx: LegContext,
) -> Result<SessionOutcome, String> {
    let mut extra_refs: Vec<&str> = cfg.extra_prompt_sections.iter().map(String::as_str).collect();
    if let Some(sec) = ctx.supplied_lessons_section.as_deref() {
        extra_refs.push(sec);
    }
    let system_prompt = build_solve_system_prompt(setup, &extra_refs);
    let mut tool_specs: Vec<ToolSpec> = router
        .registry()
        .defs()
        .iter()
        .map(|d| ToolSpec {
            name: d.id.clone(),
            description: d.description.clone(),
            input_schema: d.input_schema.clone(),
        })
        .collect();
    if cfg.human.is_some() {
        tool_specs.push(ask_user_tool_spec());
    }
    if let Some(hooks) = cfg.hooks {
        tool_specs.extend(hooks.extra_tool_specs());
    }

    let mut total_turns = ctx.total_turns_before;
    let started = Instant::now();
    loop {
        if let Some(note) = ctx.pending_repair_prompt.take() {
            ctx.messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: note }],
            });
        }
        if cfg.cancel.is_some_and(CancelToken::is_cancelled) {
            return pause_solve(
                audit,
                ctx.leg,
                ctx.start_turn,
                total_turns,
                PauseTrigger {
                    reason: PauseReason::User,
                    message: "stopped by user".into(),
                    open_items: Vec::new(),
                },
            );
        }
        if let Some(trigger) = gate.check_pause_trigger(audit.session)? {
            return pause_solve(audit, ctx.leg, ctx.start_turn, total_turns, trigger);
        }

        if let Some(h) = cfg.human {
            inject_pending_steers(audit, h, &mut ctx.messages)?;
        }

        let estimated = estimate_prompt_chars(&system_prompt, &ctx.messages, &tool_specs);
        if cfg.context_window_chars > 0
            && estimated.saturating_mul(100) >= cfg.context_window_chars.saturating_mul(AUTO_CHECKPOINT_PERCENT)
        {
            let events = audit.ledger.read_session(audit.session).map_err(|e| e.to_string())?;
            let (brief, sha) = write_checkpoint(audit, &events, ctx.leg, "context", cfg.brief_budget_chars)?;
            ctx.leg = ctx.leg.saturating_add(1);
            let resume_ev =
                emit_brief_resume(audit, transport, router, cfg, setup, ctx.leg, &ctx.run_id, &brief, &sha)?;
            gate.set_leg_start_seq(resume_ev.seq);
            ctx.messages = vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: brief }],
            }];
            if let Some(trigger) = gate.check_pause_trigger(audit.session)? {
                return pause_solve(audit, ctx.leg, ctx.start_turn, total_turns, trigger);
            }
        }

        let turn = ctx.start_turn;
        let req = ModelRequest {
            model: cfg.model.clone(),
            system: Some(system_prompt.clone()),
            messages: ctx.messages.clone(),
            tools: tool_specs.clone(),
            max_tokens: cfg.max_tokens,
            reasoning_effort: cfg.reasoning_effort.clone(),
        };
        let prepared = match transport.prepare_request(&req) {
            Ok(p) => p,
            Err(err) => {
                return pause_on_transport(audit, ctx.leg, turn, total_turns, err);
            }
        };
        let req_blob = audit.put_blob(&prepared.body)?;
        audit.emit(
            "model.request",
            json!({
                "turn": turn,
                "model": cfg.model,
                "caller": SOLVE_CALLER,
                "tool_profile": PROFILE_SOLVE_V1,
                "run_id": ctx.run_id,
                "message_count": req.messages.len(),
                "max_tokens": cfg.max_tokens,
                "raw_blob": req_blob,
            }),
        )?;

        let turn_start = Instant::now();
        let resp_res = {
            let mut obs = AuditStreamObserver { audit, cancel: cfg.cancel };
            transport.send_prepared_streaming(&prepared, &mut obs)
        };
        let resp = match resp_res {
            Ok(r) => r,
            Err(err) if err.kind == TransportErrorKind::Cancelled => {
                return pause_solve(
                    audit,
                    ctx.leg,
                    turn,
                    total_turns,
                    PauseTrigger {
                        reason: PauseReason::User,
                        message: "stopped by user".into(),
                        open_items: Vec::new(),
                    },
                );
            }
            Err(err) => {
                return pause_on_transport(audit, ctx.leg, turn, total_turns, err);
            }
        };
        total_turns = total_turns.saturating_add(1);
        ctx.start_turn = turn.saturating_add(1);
        let duration_ms = turn_start.elapsed().as_millis() as u64;

        let resp_blob = audit.put_blob(&resp.raw_body)?;
        audit.emit(
            "model.response",
            json!({
                "turn": turn,
                "response_id": resp.id,
                "model": resp.model,
                "stop_reason": resp.stop_reason.as_str(),
                "usage": resp.usage,
                "duration_ms": duration_ms,
                "raw_blob": resp_blob,
            }),
        )?;
        record_content_blocks(audit, &resp.content)?;

        if resp.stop_reason == StopReason::Refusal {
            let text = collect_text(&resp.content);
            audit.emit("model.refusal", json!({ "turn": turn, "response_id": resp.id, "text": text }))?;
            audit.emit(
                "session.end",
                json!({
                    "outcome": "refusal",
                    "stop_reason": "refusal",
                    "num_turns": total_turns,
                    "last_turn": turn,
                    "duration_ms": started.elapsed().as_millis() as u64,
                }),
            )?;
            return Ok(SessionOutcome::Refusal { turns: total_turns, text });
        }

        if let Some(reason) = validate_turn(&resp) {
            return pause_solve(
                audit,
                ctx.leg,
                turn,
                total_turns,
                PauseTrigger {
                    reason: PauseReason::TransportError,
                    message: reason,
                    open_items: Vec::new(),
                },
            );
        }

        match resp.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => {
                let stop_str = resp.stop_reason.as_str().to_string();
                audit.emit(
                    "session.end",
                    json!({
                        "outcome": "completed",
                        "stop_reason": stop_str,
                        "num_turns": total_turns,
                        "last_turn": turn,
                        "duration_ms": started.elapsed().as_millis() as u64,
                    }),
                )?;
                return Ok(SessionOutcome::Completed { turns: total_turns, stop_reason: stop_str });
            }
            StopReason::ToolUse => {
                ctx.messages.push(Message { role: Role::Assistant, content: resp.content.clone() });
                match dispatch_solve_tools(audit, router, gate, cfg, &mut ctx, turn, &resp.content)? {
                    DispatchStep::Continue(results) => {
                        ctx.messages.push(Message { role: Role::User, content: results });
                    }
                    DispatchStep::Pause(trigger) => {
                        return pause_solve(audit, ctx.leg, turn, total_turns, trigger);
                    }
                }
            }
            StopReason::Refusal | StopReason::MaxTokens | StopReason::PauseTurn | StopReason::Other(_) => {
                unreachable!("handled by validate_turn or Refusal branch");
            }
        }
    }
}

#[cfg(test)]
#[path = "solve_loop_tests.rs"]
mod tests;
