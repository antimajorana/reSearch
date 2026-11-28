//! Tests for `solve_loop.rs`: R12 (`gate_bounces`), R16 (never `session.abort`), all 9
//! `solve.paused` triggers, auto-checkpoint at 60%, D6 1-round repair, worker restart restage,
//! and S14/S20 host hook seams.

use super::{
    resume_solve_loop, run_solve_loop, SolveHostHooks, SolveLoopConfig, SolveWorkerRuntime,
    SOLVE_CALLER,
};
use crate::runner::{NoopSink, SessionOutcome};
use crate::transport::{
    CancelToken, ContentBlock, ModelRequest, ModelResponse, StopReason, StreamObserver, ToolSpec,
    Transport, TransportError, TransportErrorKind, TransportInfo,
};
use ledger::Ledger;
use research_tools::solve_checks::{CheckExecError, CheckExecResult, CheckProcessRunner};
use research_tools::solve_exprs::{CanonicalizedExpr, CheckCanonicalizer};
use research_tools::solve_gates::{solve_check_host_v1, solve_gate_chain, SolveGate};
use research_tools::solve_model::{
    compute_math_sha256, event_kinds, HandleRef, PauseReason, SolveExpr,
};
use research_tools::solve_state::SolveState;
use research_tools::tool_profile::{
    register_profile, solve_v1, ProfileHostTools, PROFILE_SOLVE_V1,
};
use research_tools::ResearchEnv;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tools::{
    ApprovalReply, ApprovalRequest, Approver, CallCtx, Registry, Router, ToolHandler, WorkerEpoch,
};

struct MockTransport {
    replies: Mutex<VecDeque<Result<ModelResponse, TransportError>>>,
    seen_requests: Mutex<Vec<ModelRequest>>,
}

impl MockTransport {
    fn new(replies: Vec<Result<ModelResponse, TransportError>>) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from(replies)),
            seen_requests: Mutex::new(Vec::new()),
        }
    }
}

impl Transport for MockTransport {
    fn info(&self) -> TransportInfo {
        TransportInfo {
            provider: "anthropic-messages",
            endpoint: "http://127.0.0.1:0/v1/messages".to_string(),
            api_key_env: "MOCK_KEY".to_string(),
        }
    }

    fn prepare_request(&self, req: &ModelRequest) -> Result<crate::PreparedRequest, TransportError> {
        self.seen_requests.lock().unwrap().push(req.clone());
        Ok(crate::PreparedRequest {
            body: serde_json::to_vec(&json!({ "model": req.model, "messages": req.messages.len() })).unwrap(),
        })
    }

    fn send_prepared(&self, _req: &crate::PreparedRequest) -> Result<ModelResponse, TransportError> {
        self.replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            Err(TransportError {
                kind: TransportErrorKind::HttpStatus,
                status: None,
                message: "unexpected extra call".into(),
                raw_body: None,
            })
        })
    }

    fn send_prepared_streaming(
        &self,
        req: &crate::PreparedRequest,
        _observer: &mut dyn StreamObserver,
    ) -> Result<ModelResponse, TransportError> {
        self.send_prepared(req)
    }
}

fn tool_resp(id: &str, name: &str, input: Value) -> Result<ModelResponse, TransportError> {
    Ok(ModelResponse {
        id: Some(format!("resp_{id}")),
        model: "claude-sonnet-4-5".into(),
        stop_reason: StopReason::ToolUse,
        content: vec![ContentBlock::ToolUse { id: id.into(), name: name.into(), input }],
        usage: json!({ "input_tokens": 100, "output_tokens": 20 }),
        raw_body: b"{}".to_vec(),
    })
}

fn end_resp(text: &str) -> Result<ModelResponse, TransportError> {
    Ok(ModelResponse {
        id: Some("resp_end".into()),
        model: "claude-sonnet-4-5".into(),
        stop_reason: StopReason::EndTurn,
        content: vec![ContentBlock::Text { text: text.into() }],
        usage: json!({ "input_tokens": 50, "output_tokens": 10 }),
        raw_body: b"{}".to_vec(),
    })
}

struct MockCanonicalizer;
impl CheckCanonicalizer for MockCanonicalizer {
    fn canonicalize(&self, raw_srepr: &str) -> Result<CanonicalizedExpr, String> {
        Ok(CanonicalizedExpr {
            portable: true,
            srepr: raw_srepr.into(),
            srepr_sha256: ledger::sha256_hex(raw_srepr.as_bytes()),
            latex: raw_srepr.into(),
            str_preview: raw_srepr.into(),
            free_symbols: Vec::new(),
            error: None,
            rejected_constructor: None,
        })
    }
}

struct MockCheckRunner;
impl CheckProcessRunner for MockCheckRunner {
    fn run_check(
        &self,
        _name: &str,
        _handles: &BTreeMap<String, String>,
        _params: &Value,
        _timeout: Duration,
        _required_epoch: Option<u64>,
    ) -> Result<CheckExecResult, CheckExecError> {
        Ok(CheckExecResult {
            raw: json!({ "outcome": "pass", "residual": "0" }),
            epoch: 1,
            request_id: 1,
            environment: None,
        })
    }
}

struct StubEpoch(Arc<AtomicU64>, bool);
impl WorkerEpoch for StubEpoch {
    fn live_epoch(&self) -> Option<u64> {
        let v = self.0.load(Ordering::Acquire);
        (v > 0).then_some(v)
    }
    fn is_contained(&self) -> bool {
        self.1
    }
}

fn seed_setup(ledger: &Ledger, session: &str, per_leg: u32, total_calls: u32, bounces: u32) {
    ledger.append_with_hash(session, event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" })).unwrap();
    let msha = compute_math_sha256(&[], &Default::default(), &[], "");
    ledger.append_with_hash(session, event_kinds::SOLVE_SETUP, 1, &json!({
        "revision": 1,
        "statement": "Solve 1+1",
        "task_kind": "calculate",
        "target": { "description": "2", "form": "closed_form" },
        "done": [{ "id": "d1", "kind": "no_open_flags", "params": {} }],
        "conventions": "", "symbols": [], "dimensions": {}, "assumptions": [], "sources": [],
        "roles": { "solver": { "provider": "anthropic", "model": "claude-sonnet-4-5" }, "verifier": { "provider": "openai", "model": "gpt-4.1" } },
        "budgets": { "max_model_calls": total_calls, "max_wall_minutes": 30, "max_model_calls_per_leg": per_leg, "max_review_loops": 6, "max_gate_bounces": bounces },
        "review": "prose_only",
        "tool_profile": "solve/1",
        "math_sha256": msha
    })).unwrap();
}

struct TestApproveAll;
impl Approver for TestApproveAll {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalReply {
        ApprovalReply { approved: true, digest: req.digest.clone(), by: "test".into() }
    }
}

struct DenySympyApprover;
impl Approver for DenySympyApprover {
    fn decide(&self, req: &ApprovalRequest) -> ApprovalReply {
        ApprovalReply { approved: false, digest: req.digest.clone(), by: "test".into() }
    }
}

fn make_solve_router(
    dir: &std::path::Path,
    session: &str,
    epoch: Arc<StubEpoch>,
    sympy_handler: Arc<dyn ToolHandler>,
    approver: Arc<dyn Approver>,
) -> (Arc<ResearchEnv>, Router, Arc<research_tools::solve_gates::SolveGate>) {
    let env = Arc::new(ResearchEnv::new(dir));
    let runner: Arc<dyn CheckProcessRunner> = Arc::new(MockCheckRunner);
    let ledger = Ledger::open(dir).unwrap();
    let seq0 = ledger.read_session(session).unwrap().last().map_or(0, |e| e.seq);
    let host = ProfileHostTools::new(epoch.clone(), sympy_handler)
        .with_solve_check(solve_check_host_v1(epoch, Arc::clone(&env), Arc::clone(&runner)));
    let mut reg = Registry::new();
    let policy = register_profile(&solve_v1(), &mut reg, Arc::clone(&env), host).unwrap();
    let (gate, chain) = solve_gate_chain(Arc::clone(&env), runner, seq0, seq0);
    let router = Router::new(reg, policy, ledger, session, approver)
        .with_tool_profile(PROFILE_SOLVE_V1)
        .with_gate(chain);
    (env, router, gate)
}

fn base_cfg<'a>(env: Arc<ResearchEnv>, gate: Arc<SolveGate>) -> SolveLoopConfig<'a> {
    SolveLoopConfig {
        model_requested: "claude-sonnet-4-5".into(),
        model: "claude-sonnet-4-5".into(),
        max_tokens: 4096,
        reasoning_effort: None,
        containment: "test".into(),
        contained: true,
        prompt: "Solve it.".into(),
        run_id: Some("run-test-1".into()),
        context_window_chars: 400_000,
        brief_budget_chars: 16_000,
        extra_prompt_sections: Vec::new(),
        env,
        solve_gate: Some(gate),
        canonicalizer: Some(&MockCanonicalizer),
        worker_runtime: None,
        hooks: None,
        human: None,
        cancel: None,
    }
}

#[test]
fn gate_bounces_r12_and_transport_error_r16_never_abort() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger, "s-r12", 48, 60, 3);
    let count = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&count);
    let sympy: Arc<dyn ToolHandler> = Arc::new(move |_ctx: &CallCtx<'_>, _args: &Value| {
        let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({
            "ok": true,
            "value": format!("x{n}"),
            "error": Value::Null,
            "exprs_out": { format!("x{n}"): format!("Symbol('x{n}')") }
        }))
    });
    let ep = Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true));
    let (env, router, gate) = make_solve_router(tmp.path(), "s-r12", ep, sympy, Arc::new(TestApproveAll));
    // 3 successful un-cited sympy runs fill G1, then 3 blocked sympy calls hit G5 (max_gate_bounces=3).
    let replies = (1..=7)
        .map(|i| tool_resp(&format!("tu_{i}"), "sympy_eval", json!({ "code": format!("x={i}") })))
        .collect();
    let tr = MockTransport::new(replies);
    let cfg = base_cfg(env, gate);
    let out = run_solve_loop(&tr, &router, &ledger, &cfg, &mut NoopSink).unwrap();
    assert_eq!(out, SessionOutcome::Paused { turns: 6, reason: "gate_bounces".into() });
    let events = ledger.read_session("s-r12").unwrap();
    assert!(!events.iter().any(|e| e.kind == "session.abort"));
    let reqs: Vec<_> = events.iter().filter(|e| e.kind == "model.request").collect();
    assert_eq!(reqs.len(), 6);
    assert_eq!(reqs[0].payload["caller"], SOLVE_CALLER);
    assert_eq!(reqs[0].payload["tool_profile"], PROFILE_SOLVE_V1);
    assert_eq!(reqs[0].payload["run_id"], "run-test-1");
    assert_eq!(SolveState::from_events(&events).run_status, research_tools::solve_model::SolveRunStatus::Paused(PauseReason::GateBounces));

    // R16: transport error -> solve.paused { transport_error }, zero session.abort.
    let ledger2 = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger2, "s-r16", 48, 60, 3);
    let sympy_ok: Arc<dyn ToolHandler> = Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({})));
    let ep2 = Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true));
    let (env2, router2, gate2) = make_solve_router(tmp.path(), "s-r16", ep2, sympy_ok, Arc::new(TestApproveAll));
    let tr2 = MockTransport::new(vec![Err(TransportError {
        kind: TransportErrorKind::HttpStatus,
        status: Some(502),
        message: "502".into(),
        raw_body: None,
    })]);
    let cfg2 = base_cfg(env2, gate2);
    let out2 = run_solve_loop(&tr2, &router2, &ledger2, &cfg2, &mut NoopSink).unwrap();
    assert_eq!(out2, SessionOutcome::Paused { turns: 0, reason: "transport_error".into() });
    let ev2 = ledger2.read_session("s-r16").unwrap();
    assert!(ev2.iter().any(|e| e.kind == "model.error"));
    assert!(!ev2.iter().any(|e| e.kind == "session.abort"));
}

#[test]
fn auto_checkpoint_60_percent_d6_repair_and_worker_restart_restage() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger, "s-auto", 48, 60, 3);
    let sympy_ok: Arc<dyn ToolHandler> = Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({ "ok": true })));
    let ep = Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true));
    let (env, router, gate) = make_solve_router(tmp.path(), "s-auto", ep, sympy_ok, Arc::new(TestApproveAll));
    let tr = MockTransport::new(vec![
        tool_resp("tu_1", "ledger_note", json!({ "text": &"a".repeat(8_000) })),
        end_resp("Continued after brief checkpoint."),
    ]);
    let mut cfg = base_cfg(env, gate);
    cfg.context_window_chars = 16_000; // baseline < 60%, turn 1 pushes >= 60% -> 1 checkpoint before turn 2
    let out = run_solve_loop(&tr, &router, &ledger, &cfg, &mut NoopSink).unwrap();
    assert_eq!(out, SessionOutcome::Completed { turns: 2, stop_reason: "end_turn".into() });
    let evs = ledger.read_session("s-auto").unwrap();
    let cps: Vec<_> = evs.iter().filter(|e| e.kind == event_kinds::SOLVE_CHECKPOINT).collect();
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].payload["reason"], "context");
    assert!(evs.iter().any(|e| e.kind == "session.resume" && e.payload["resume_mode"] == "brief"));
    let seen = tr.seen_requests.lock().unwrap();
    assert_eq!(seen.len(), 2, "zero model calls spent on summarizing");
    assert_eq!(seen[1].messages.len(), 1, "leg 2 starts fresh from resume brief only");
    assert_eq!(ledger.verify_all().unwrap().unreferenced_blobs, 0);

    // Worker restart -> restage S7 handles -> approval_needed on epoch change.
    let restaged = Arc::new(AtomicBool::new(false));
    struct Rt(Arc<AtomicU64>, Arc<AtomicBool>);
    impl SolveWorkerRuntime for Rt {
        fn worker_epoch(&self) -> Option<u64> { Some(self.0.load(Ordering::Acquire)) }
        fn restage_handles(&self, _: &Ledger, _: &str, h: &BTreeMap<HandleRef, SolveExpr>) -> Result<(), String> {
            assert!(!h.is_empty());
            self.1.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    let ledger3 = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger3, "s-wr", 48, 60, 3);
    let ep3 = Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true));
    let ep_handler = Arc::clone(&ep3);
    let calls3 = Arc::new(AtomicUsize::new(0));
    let sympy3: Arc<dyn ToolHandler> = Arc::new(move |_: &CallCtx<'_>, _: &Value| {
        if calls3.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(json!({ "ok": true, "value": "42", "error": Value::Null, "exprs_out": { "ans": "Integer(42)" } }))
        } else {
            ep_handler.0.store(2, Ordering::Release);
            Err("worker timed out and restarted".into())
        }
    });
    let (env3, router3, gate3) = make_solve_router(tmp.path(), "s-wr", Arc::clone(&ep3), sympy3, Arc::new(TestApproveAll));
    let rt = Rt(Arc::clone(&ep3.0), Arc::clone(&restaged));
    let mut cfg3 = base_cfg(env3, gate3);
    cfg3.worker_runtime = Some(&rt);
    let tr3 = MockTransport::new(vec![
        tool_resp("w1", "sympy_eval", json!({ "code": "save_expr('ans', 42)" })),
        tool_resp("w2", "sympy_eval", json!({ "code": "2+2" })),
    ]);
    let out3 = run_solve_loop(&tr3, &router3, &ledger3, &cfg3, &mut NoopSink).unwrap();
    assert_eq!(out3, SessionOutcome::Paused { turns: 2, reason: "approval_needed".into() });
    assert!(restaged.load(Ordering::SeqCst));

    // D6 repair round + approval_needed denial + user cancel + S14/S20 host hooks seam.
    let ledger4 = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger4, "s-d6", 48, 60, 3);
    let msha = SolveState::from_events(&ledger4.read_session("s-d6").unwrap()).setup.unwrap().math_sha256;
    ledger4.append_with_hash("s-d6", event_kinds::SOLVE_SETUP, 1, &json!({
        "revision": 2, "statement": "s", "task_kind": "calculate",
        "target": { "description": "t", "form": "closed_form" },
        "done": [{ "id": "d1", "kind": "final_tier_at_least", "params": { "tier": "verified" } }],
        "conventions": "", "symbols": [], "dimensions": {}, "assumptions": [], "sources": [],
        "roles": { "solver": { "provider": "anthropic", "model": "claude-sonnet-4-5" }, "verifier": { "provider": "openai", "model": "gpt-4.1" } },
        "budgets": { "max_model_calls": 60, "max_wall_minutes": 30, "max_model_calls_per_leg": 48, "max_review_loops": 6, "max_gate_bounces": 3 },
        "review": "prose_only", "tool_profile": "solve/1", "math_sha256": msha
    })).unwrap();
    let hook_called = Arc::new(AtomicBool::new(false));
    struct SeamHook(Arc<AtomicBool>);
    impl SolveHostHooks for SeamHook {
        fn extra_tool_specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec { name: "review_steps".into(), description: "s14".into(), input_schema: json!({}) }]
        }
        fn handle_host_tool(&self, _: &Ledger, _: &str, name: &str, _: &str, _: &Value, _: Option<&CancelToken>) -> Option<Result<(bool, String), String>> {
            (name == "review_steps").then(|| { self.0.store(true, Ordering::SeqCst); Ok((false, "{}".into())) })
        }
    }
    let ep4 = Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true));
    let (env4, router4, gate4) = make_solve_router(tmp.path(), "s-d6", ep4, Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))), Arc::new(TestApproveAll));
    let seam = SeamHook(Arc::clone(&hook_called));
    let mut cfg4 = base_cfg(env4, gate4);
    cfg4.hooks = Some(&seam);
    let tr4 = MockTransport::new(vec![
        tool_resp("d0", "review_steps", json!({})),
        tool_resp("d1", "step_record", json!({ "entry_type": "assumption", "summary": "given", "details": "d", "depends_on": [] })),
        tool_resp("d1b", "step_record", json!({ "entry_type": "final_answer", "summary": "s", "details": "d", "depends_on": ["e-1"] })),
        tool_resp("d2", "solve_finish", json!({ "final_entry_id": "e-2", "summary_markdown": "Answer", "outcome": "finished" })),
        tool_resp("d3", "solve_finish", json!({ "final_entry_id": "e-2", "summary_markdown": "Answer", "outcome": "finished" })),
        end_resp("Settled with open items."),
    ]);
    let out4 = run_solve_loop(&tr4, &router4, &ledger4, &cfg4, &mut NoopSink).unwrap();
    assert_eq!(out4, SessionOutcome::Completed { turns: 6, stop_reason: "end_turn".into() });
    assert!(hook_called.load(Ordering::SeqCst));
    assert!(tr4.seen_requests.lock().unwrap()[4].messages.iter().any(|m| format!("{:?}", m.content).contains("solve_repair_round")));

    // Cancel & explicit approval denial
    let cancel = CancelToken::new();
    cancel.cancel();
    let ledger5 = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger5, "s-cancel", 48, 60, 3);
    let (env5, router5, gate5) = make_solve_router(tmp.path(), "s-cancel", Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)), Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))), Arc::new(DenySympyApprover));
    let mut cfg5 = base_cfg(env5.clone(), gate5.clone());
    cfg5.cancel = Some(&cancel);
    assert_eq!(run_solve_loop(&MockTransport::new(vec![]), &router5, &ledger5, &cfg5, &mut NoopSink).unwrap(), SessionOutcome::Paused { turns: 0, reason: "user".into() });

    let ledger6 = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger6, "s-app", 48, 60, 3);
    let (env6, router6, gate6) = make_solve_router(tmp.path(), "s-app", Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)), Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))), Arc::new(DenySympyApprover));
    let cfg6 = base_cfg(env6, gate6);
    let tr6 = MockTransport::new(vec![tool_resp("a1", "sympy_eval", json!({ "code": "1+1" }))]);
    assert_eq!(run_solve_loop(&tr6, &router6, &ledger6, &cfg6, &mut NoopSink).unwrap(), SessionOutcome::Paused { turns: 1, reason: "approval_needed".into() });
    // Brief resume after approval_needed
    let ledger6_ref = Ledger::open(tmp.path()).unwrap();
    let tr6_res = MockTransport::new(vec![end_resp("Resumed cleanly.")]);
    let (_, router6r, gate6r) = make_solve_router(tmp.path(), "s-app", Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)), Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))), Arc::new(TestApproveAll));
    let mut cfg6r = base_cfg(Arc::new(ResearchEnv::new(tmp.path())), gate6r);
    cfg6r.prompt = "Approved, continue.".into();
    let out6r = resume_solve_loop(&tr6_res, &router6r, &ledger6_ref, &cfg6r, &mut NoopSink).unwrap();
    assert_eq!(out6r, SessionOutcome::Completed { turns: 1, stop_reason: "end_turn".into() });
}

#[test]
fn fresh_run_supplies_matching_lessons_once_and_resume_uses_frozen_brief() {
    use research_tools::lessons_capture::{
        LessonScope, LessonSourceKind, SaveLessonRequest, UserLessonsStore,
    };
    use research_tools::lessons_prompt::LESSONS_PROMPT_HEADING;
    use research_tools::paper_key::{Mode, NodeRef};

    let tmp = tempfile::tempdir().unwrap();
    let store = UserLessonsStore::open(tmp.path()).unwrap();
    let origin = NodeRef {
        session: "s-prev".into(),
        mode: Mode::Solve,
        id: "e-1".into(),
        paper: None,
        node_fp: None,
    };
    let solve_note = store
        .save_lesson(SaveLessonRequest {
            text: "Use mostly-minus metric (+,-,-,-) here.".into(),
            scope: LessonScope::mode(Mode::Solve),
            from: origin.clone(),
            source_kind: LessonSourceKind::StepCorrection,
            model_summary: false,
            created_ms: Some(1_000),
        })
        .unwrap();
    let _explore_only = store
        .save_lesson(SaveLessonRequest {
            text: "Explore-only note must stay excluded from Solve.".into(),
            scope: LessonScope::mode(Mode::Explore),
            from: origin,
            source_kind: LessonSourceKind::ExploreCorrection,
            model_summary: false,
            created_ms: Some(2_000),
        })
        .unwrap();

    let ledger = Ledger::open(tmp.path()).unwrap();
    seed_setup(&ledger, "s-les", 48, 60, 3);
    let (_, router, gate) = make_solve_router(
        tmp.path(),
        "s-les",
        Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)),
        Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))),
        Arc::new(DenySympyApprover),
    );
    let cfg = base_cfg(Arc::new(ResearchEnv::new(tmp.path())), gate);
    let tr = MockTransport::new(vec![tool_resp("a1", "sympy_eval", json!({ "code": "1+1" }))]);
    assert_eq!(
        run_solve_loop(&tr, &router, &ledger, &cfg, &mut NoopSink).unwrap(),
        SessionOutcome::Paused { turns: 1, reason: "approval_needed".into() }
    );

    let reqs = tr.seen_requests.lock().unwrap();
    let sys = reqs[0].system.as_deref().unwrap();
    assert!(sys.contains(LESSONS_PROMPT_HEADING));
    assert!(sys.contains("Use mostly-minus metric (+,-,-,-) here."));
    assert!(!sys.contains("Explore-only note"));
    assert_eq!(store.get_lesson(&solve_note.id).unwrap().unwrap().uses, 1);

    // Mutate and delete the live lesson before resuming; resume must read frozen ledger only.
    store.delete_lesson(&solve_note.id).unwrap();
    let (_, router_r, gate_r) = make_solve_router(
        tmp.path(),
        "s-les",
        Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)),
        Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))),
        Arc::new(TestApproveAll),
    );
    let mut cfg_r = base_cfg(Arc::new(ResearchEnv::new(tmp.path())), gate_r);
    cfg_r.prompt = "Continue.".into();
    let tr_r = MockTransport::new(vec![end_resp("Done.")]);
    assert_eq!(
        resume_solve_loop(&tr_r, &router_r, &ledger, &cfg_r, &mut NoopSink).unwrap(),
        SessionOutcome::Completed { turns: 1, stop_reason: "end_turn".into() }
    );
    let res_reqs = tr_r.seen_requests.lock().unwrap();
    let brief_msg = format!("{:?}", res_reqs[0].messages);
    assert!(brief_msg.contains("Use mostly-minus metric (+,-,-,-) here."));
    assert_eq!(store.get_lesson(&solve_note.id).unwrap(), None);
    let report = ledger.verify_all().unwrap();
    assert_eq!(report.unreferenced_blobs, 0);
}

#[test]
fn run_solve_loop_empty_or_whitespace_prompt_seeds_first_user_turn_from_setup_statement() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(tmp.path()).unwrap();
    for (idx, raw_prompt) in [(1, ""), (2, "  \n\t  ")] {
        let sid = format!("s-empty-prompt-{idx}");
        seed_setup(&ledger, &sid, 48, 60, 3);
        let (_, router, gate) = make_solve_router(
            tmp.path(),
            &sid,
            Arc::new(StubEpoch(Arc::new(AtomicU64::new(1)), true)),
            Arc::new(|_: &CallCtx<'_>, _: &Value| Ok(json!({}))),
            Arc::new(TestApproveAll),
        );
        let mut cfg = base_cfg(Arc::new(ResearchEnv::new(tmp.path())), gate);
        cfg.prompt = raw_prompt.into();
        let tr = MockTransport::new(vec![end_resp("1+1 = 2.")]);
        let out = run_solve_loop(&tr, &router, &ledger, &cfg, &mut NoopSink).unwrap();
        assert_eq!(out, SessionOutcome::Completed { turns: 1, stop_reason: "end_turn".into() });

        let reqs = tr.seen_requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let last_msg = reqs[0].messages.last().expect("first request must carry user message");
        assert_eq!(last_msg.role, crate::transport::Role::User);
        let ContentBlock::Text { text } = &last_msg.content[0] else {
            panic!("expected user text block, got {:?}", last_msg.content);
        };
        assert!(!text.trim().is_empty());
        assert!(text.contains("Solve 1+1"), "first user turn must include setup statement: {text}");

        // Must also serialize cleanly for the ChatGPT Responses API route without `input: []`.
        let wire: Value =
            serde_json::from_slice(&crate::responses::serialize_responses_request(&reqs[0]).unwrap()).unwrap();
        let input = wire["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert!(input[0]["content"].as_str().unwrap_or("").contains("Solve 1+1"));

        let evs = ledger.read_session(&sid).unwrap();
        let start_ev = evs.iter().find(|e| e.kind == "session.start").unwrap();
        assert_eq!(start_ev.payload["prompt"], raw_prompt);
        assert_eq!(
            start_ev.payload["first_user_text_sha256"].as_str().unwrap(),
            ledger::sha256_hex(text.as_bytes()),
        );
    }
}
