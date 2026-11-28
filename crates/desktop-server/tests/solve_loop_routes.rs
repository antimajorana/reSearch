//! HTTP integration tests for S12d Solve continue & interrupted reconciliation routes.

mod common;

use common::{raw_http, test_cfg, MockHttp, MockReply};
use desktop_server::start_desktop_server;
use ledger::Ledger;
use research_tools::solve_model::{compute_entry_sha256, compute_math_sha256, event_kinds};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn setup_payload() -> Value {
    let msha = compute_math_sha256(&[], &Default::default(), &[], "");
    json!({
        "revision": 1,
        "statement": "Compute sum",
        "task_kind": "calculate",
        "target": { "description": "2", "form": "closed_form" },
        "done": [{ "id": "d1", "kind": "no_open_flags", "params": {} }],
        "conventions": "",
        "symbols": [],
        "dimensions": {},
        "assumptions": [],
        "sources": [],
        "roles": {
            "solver": { "provider": "anthropic", "model": "claude-sonnet-4-5" },
            "verifier": { "provider": "openai", "model": "gpt-4.1" }
        },
        "budgets": {
            "max_model_calls": 60,
            "max_wall_minutes": 30,
            "max_model_calls_per_leg": 48,
            "max_review_loops": 6,
            "max_gate_bounces": 3
        },
        "review": "prose_only",
        "tool_profile": "solve/1",
        "math_sha256": msha
    })
}

fn anthropic_json_reply(id: &str, text: &str) -> MockReply {
    MockReply {
        status: 200,
        content_type: "application/json",
        body: serde_json::to_vec(&json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": text }],
            "usage": { "input_tokens": 25, "output_tokens": 12 }
        }))
        .unwrap(),
    }
}

#[test]
fn solve_continue_runs_background_leg_and_reconcile_handles_interrupted_sessions_without_get_mutation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let ledger = Ledger::open(&data_dir).unwrap();

    // Seed an interrupted Solve session (dangling open leg without solve.paused/done).
    ledger.append_with_hash("s-solve-int", event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" })).unwrap();
    ledger.append_with_hash("s-solve-int", event_kinds::SOLVE_SETUP, 1, &setup_payload()).unwrap();
    ledger.append_with_hash("s-solve-int", "session.start", 1, &json!({
        "route": "api",
        "provider": "anthropic-messages",
        "model": "claude-sonnet-4-5",
        "prompt": "Solve it",
        "tool_profile": "solve/1",
        "leg": 1
    })).unwrap();
    ledger.append_with_hash("s-solve-int", "model.request", 1, &json!({ "turn": 1, "caller": "solver" })).unwrap();

    // Seed a paused Solve session ready for `POST /solve/continue`.
    ledger.append_with_hash("s-solve-cont", event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" })).unwrap();
    ledger.append_with_hash("s-solve-cont", event_kinds::SOLVE_SETUP, 1, &setup_payload()).unwrap();
    ledger.append_with_hash("s-solve-cont", "session.start", 1, &json!({
        "route": "api",
        "provider": "anthropic-messages",
        "model": "claude-sonnet-4-5",
        "prompt": "Solve it",
        "tool_profile": "solve/1",
        "leg": 1
    })).unwrap();
    ledger.append_with_hash("s-solve-cont", event_kinds::SOLVE_PAUSED, 1, &json!({
        "reason": "leg_cap", "leg": 1, "turn": 1, "model_calls_used": 1, "open_items": []
    })).unwrap();

    // Configure provider key for loopback MockHttp.
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("providers.json"),
        serde_json::to_vec(&json!({
            "providers": { "anthropic": { "api_key": "sk-test-solve-123" } }
        }))
        .unwrap(),
    )
    .unwrap();
    let mock = MockHttp::start(vec![anthropic_json_reply("msg_c1", "Continued leg finished cleanly.")]);
    let srv = start_desktop_server(test_cfg(&data_dir, None, Some(mock.base_url.clone()), None)).unwrap();
    let port = srv.addr.port();
    let auth = [("X-ReSearch-Token", srv.info.token.as_str()), ("Content-Type", "application/json")];

    // Startup reconciliation recorded `solve.paused { reason: "interrupted" }` on `s-solve-int`!
    let evs_int = ledger.read_session("s-solve-int").unwrap();
    assert_eq!(evs_int.last().unwrap().kind, event_kinds::SOLVE_PAUSED);
    assert_eq!(evs_int.last().unwrap().payload["reason"], "interrupted");
    let count_before_get = evs_int.len();

    // `GET /solve/view` never mutates ledger (THREAT-MODEL.md:333).
    let (st, _, view_raw) = raw_http(port, "GET", "/api/sessions/s-solve-int/solve/view", &auth, b"");
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&view_raw));
    assert_eq!(ledger.read_session("s-solve-int").unwrap().len(), count_before_get);

    // Explicit `POST /solve/reconcile` returns 200 `{ reconciled: false }` when already paused,
    // and `{ reconciled: true }` when a dangling leg is introduced after boot.
    let (st, _, rec1) = raw_http(port, "POST", "/api/sessions/s-solve-int/solve/reconcile", &auth, b"{}");
    assert_eq!(st, 200);
    assert_eq!(serde_json::from_slice::<Value>(&rec1).unwrap()["reconciled"], false);

    ledger.append_with_hash("s-solve-int", "session.resume", 1, &json!({ "leg": 2 })).unwrap();
    let (st, _, view2_raw) = raw_http(port, "GET", "/api/sessions/s-solve-int/solve/view", &auth, b"");
    assert_eq!(st, 200);
    let view2: Value = serde_json::from_slice(&view2_raw).unwrap();
    assert_eq!(view2["run_state"]["state"], "interrupted");
    let seq_before_rec2 = ledger.read_session("s-solve-int").unwrap().len();
    // Double check GET appended zero events.
    let _ = raw_http(port, "GET", "/api/sessions/s-solve-int/solve/view", &auth, b"");
    assert_eq!(ledger.read_session("s-solve-int").unwrap().len(), seq_before_rec2);

    let (st, _, rec2) = raw_http(port, "POST", "/api/sessions/s-solve-int/solve/reconcile", &auth, b"{}");
    assert_eq!(st, 200);
    assert_eq!(serde_json::from_slice::<Value>(&rec2).unwrap()["reconciled"], true);

    // `POST /solve/continue` accepts 202 and completes leg 2 cleanly in background.
    let cont_body = serde_json::to_vec(&json!({ "note": "Keep going", "provider": "anthropic", "model": "claude-sonnet-4-5" })).unwrap();
    let (st, _, cont_raw) = raw_http(port, "POST", "/api/sessions/s-solve-cont/solve/continue", &auth, &cont_body);
    assert_eq!(st, 202, "{}", String::from_utf8_lossy(&cont_raw));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let evs = ledger.read_session("s-solve-cont").unwrap();
        if evs.iter().any(|e| e.kind == "session.end") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let evs_cont = ledger.read_session("s-solve-cont").unwrap();
    assert!(evs_cont.iter().any(|e| e.kind == event_kinds::SOLVE_CHECKPOINT && e.payload["reason"] == "continue"));
    assert!(evs_cont.iter().any(|e| e.kind == "session.resume" && e.payload["resume_mode"] == "brief"));
    assert!(evs_cont.iter().any(|e| e.kind == "session.end"));
    assert!(!evs_cont.iter().any(|e| e.kind == "session.abort"));
    srv.shutdown();
    mock.finish();
}

#[test]
fn solve_post_messages_dispatches_to_solve_loop_and_pauses_on_transport_error_r16() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let ledger = Ledger::open(&data_dir).unwrap();
    ledger.append_with_hash("s-solve-msg", event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" })).unwrap();
    ledger.append_with_hash("s-solve-msg", event_kinds::SOLVE_SETUP, 1, &setup_payload()).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("providers.json"),
        serde_json::to_vec(&json!({
            "providers": { "anthropic": { "api_key": "sk-test-solve-456" } }
        }))
        .unwrap(),
    )
    .unwrap();

    let mock = MockHttp::start(vec![MockReply {
        status: 500,
        content_type: "application/json",
        body: br#"{"error":{"message":"boom"}}"#.to_vec(),
    }]);
    let srv = start_desktop_server(test_cfg(&data_dir, None, Some(mock.base_url.clone()), None)).unwrap();
    let port = srv.addr.port();
    let auth = [("X-ReSearch-Token", srv.info.token.as_str()), ("Content-Type", "application/json")];

    let msg_body = serde_json::to_vec(&json!({
        "prompt": "Compute sum",
        "provider": "anthropic",
        "model": "claude-sonnet-4-5"
    }))
    .unwrap();
    let (st, _, raw) = raw_http(port, "POST", "/api/sessions/s-solve-msg/messages", &auth, &msg_body);
    assert_eq!(st, 202, "{}", String::from_utf8_lossy(&raw));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let evs = ledger.read_session("s-solve-msg").unwrap();
        if evs.iter().any(|e| e.kind == event_kinds::SOLVE_PAUSED) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let evs = ledger.read_session("s-solve-msg").unwrap();
    assert!(evs.iter().any(|e| e.kind == "session.start" && e.payload["tool_profile"] == "solve/1"));
    assert!(evs.iter().any(|e| e.kind == "model.request" && e.payload["caller"] == "solver"));
    assert!(evs.iter().any(|e| e.kind == event_kinds::SOLVE_PAUSED && e.payload["reason"] == "transport_error"));
    assert!(!evs.iter().any(|e| e.kind == "session.abort"));
    srv.shutdown();
    mock.finish();
}

fn anthropic_tool_reply(id: &str, tool_id: &str, name: &str, input: Value) -> MockReply {
    MockReply {
        status: 200,
        content_type: "application/json",
        body: serde_json::to_vec(&json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "stop_reason": "tool_use",
            "content": [{ "type": "tool_use", "id": tool_id, "name": name, "input": input }],
            "usage": { "input_tokens": 30, "output_tokens": 20 }
        }))
        .unwrap(),
    }
}

fn openai_tool_reply(id: &str, call_id: &str, name: &str, args: Value) -> MockReply {
    MockReply {
        status: 200,
        content_type: "application/json",
        body: serde_json::to_vec(&json!({
            "id": id,
            "object": "chat.completion",
            "model": "gpt-4.1",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": args.to_string() }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 40, "completion_tokens": 18, "total_tokens": 58 }
        }))
        .unwrap(),
    }
}

#[test]
fn solve_finish_runs_desktop_verifier_before_done_criteria_evaluation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let ledger = Ledger::open(&data_dir).unwrap();
    let mut setup = setup_payload();
    setup["done"] = json!([
        { "id": "d1", "kind": "closure_tier_at_least", "params": { "tier": "model_agreed" } },
        { "id": "d2", "kind": "no_open_flags", "params": {} }
    ]);
    ledger
        .append_with_hash("s-solve-s14-hook", event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" }))
        .unwrap();
    ledger
        .append_with_hash("s-solve-s14-hook", event_kinds::SOLVE_SETUP, 1, &setup)
        .unwrap();
    let e1_sha = compute_entry_sha256(None, "Given summands", "Summands are 1 and 1.");
    ledger
        .append_with_hash(
            "s-solve-s14-hook",
            event_kinds::PROOF_ENTRY,
            1,
            &json!({
                "entry_id": "e-1",
                "entry_type": "assumption",
                "summary": "Given summands",
                "details": "Summands are 1 and 1.",
                "depends_on": [],
                "entry_sha256": e1_sha,
                "origin": "given"
            }),
        )
        .unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("providers.json"),
        serde_json::to_vec(&json!({
            "providers": {
                "anthropic": { "api_key": "sk-test-solve-s14" },
                "openai": { "api_key": "sk-test-verifier-s14" }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mock = MockHttp::start(vec![
        anthropic_tool_reply(
            "msg_1",
            "tu_step",
            "step_record",
            json!({
                "entry_type": "result",
                "depends_on": ["e-1"],
                "summary": "Sum equals two",
                "details": "Adding 1 and 1 gives 2."
            }),
        ),
        anthropic_tool_reply(
            "msg_2",
            "tu_finish",
            "solve_finish",
            json!({
                "final_entry_id": "e-2",
                "summary_markdown": "Step e-2 concludes the sum is 2."
            }),
        ),
        openai_tool_reply(
            "chatcmpl_1",
            "call_v1",
            "report_verdict",
            json!({
                "verdicts": [{
                    "entry_id": "e-2",
                    "verdict": "accepted",
                    "summary": "Prose arithmetic is valid.",
                    "checks_cited": []
                }]
            }),
        ),
        anthropic_json_reply("msg_3", "Finished."),
    ]);

    let srv = start_desktop_server(test_cfg(&data_dir, None, Some(mock.base_url.clone()), None)).unwrap();
    let port = srv.addr.port();
    let auth = [("X-ReSearch-Token", srv.info.token.as_str()), ("Content-Type", "application/json")];

    let msg_body = serde_json::to_vec(&json!({
        "prompt": "Compute sum",
        "provider": "anthropic",
        "model": "claude-sonnet-4-5"
    }))
    .unwrap();
    let (st, _, raw) = raw_http(port, "POST", "/api/sessions/s-solve-s14-hook/messages", &auth, &msg_body);
    assert_eq!(st, 202, "{}", String::from_utf8_lossy(&raw));

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let evs = ledger.read_session("s-solve-s14-hook").unwrap();
        if evs.iter().any(|e| e.kind == "session.end" || e.kind == event_kinds::SOLVE_PAUSED) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let evs = ledger.read_session("s-solve-s14-hook").unwrap();
    let review_ev = evs
        .iter()
        .find(|e| {
            e.kind == event_kinds::PROOF_VERIFICATION
                && e.payload["target_entry_id"] == "e-2"
                && e.payload["verdict"] == "accepted"
        })
        .unwrap_or_else(|| panic!("expected finish-time PROOF_VERIFICATION in {evs:#?}"));
    assert_eq!(review_ev.payload["independence"], "different_provider");

    let done_ev = evs
        .iter()
        .find(|e| e.kind == event_kinds::SOLVE_DONE)
        .unwrap_or_else(|| panic!("expected SOLVE_DONE in {evs:#?}"));
    assert!(review_ev.seq < done_ev.seq, "verifier review must precede solve.done");
    assert_eq!(done_ev.payload["final_entry_id"], "e-2");
    assert_eq!(done_ev.payload["status"], "finished");
    assert!(evs.iter().any(|e| e.kind == "session.end"));
    assert!(!evs.iter().any(|e| e.kind == "session.abort"));
    srv.shutdown();
    mock.finish();
}

#[test]
fn fresh_solve_continue_with_empty_body_uses_setup_statement_and_solver_role() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let ledger = Ledger::open(&data_dir).unwrap();
    let mut setup = setup_payload();
    setup["statement"] = json!("Compute the integral \\int_0^1 x^2\\,dx.");
    setup["roles"]["solver"] = json!({ "provider": "openai", "model": "gpt-4.1" });
    ledger
        .append_with_hash("s-solve-fresh", event_kinds::SOLVE_STARTED, 1, &json!({ "origin": "new" }))
        .unwrap();
    ledger
        .append_with_hash("s-solve-fresh", event_kinds::SOLVE_SETUP, 1, &setup)
        .unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join("providers.json"),
        serde_json::to_vec(&json!({
            "providers": { "openai": { "api_key": "sk-test-openai-fresh" } }
        }))
        .unwrap(),
    )
    .unwrap();

    let mock = MockHttp::start(vec![MockReply {
        status: 200,
        content_type: "application/json",
        body: serde_json::to_vec(&json!({
            "id": "chatcmpl_fresh_1",
            "object": "chat.completion",
            "model": "gpt-4.1",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "1/3" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 30, "completion_tokens": 5, "total_tokens": 35 }
        }))
        .unwrap(),
    }]);
    let srv = start_desktop_server(test_cfg(&data_dir, None, Some(mock.base_url.clone()), None)).unwrap();
    let port = srv.addr.port();
    let auth = [("X-ReSearch-Token", srv.info.token.as_str()), ("Content-Type", "application/json")];

    let (st, _, raw) = raw_http(port, "POST", "/api/sessions/s-solve-fresh/solve/continue", &auth, b"{}");
    assert_eq!(st, 202, "{}", String::from_utf8_lossy(&raw));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let evs = ledger.read_session("s-solve-fresh").unwrap();
        if evs.iter().any(|e| e.kind == "session.end" || e.kind == event_kinds::SOLVE_PAUSED) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let evs = ledger.read_session("s-solve-fresh").unwrap();
    assert!(evs.iter().any(|e| e.kind == "session.end"), "expected completion, got {evs:#?}");
    let req_ev = evs.iter().find(|e| e.kind == "model.request").expect("model.request");
    assert_eq!(req_ev.payload["model"], "gpt-4.1");
    let blob_id = req_ev.payload["raw_blob"].as_str().expect("raw_blob");
    let sent_body: Value = serde_json::from_slice(&ledger.get_blob(blob_id).unwrap()).unwrap();
    let msgs = sent_body["messages"].as_array().expect("messages array");
    let user_msg = msgs.iter().rev().find(|m| m["role"] == "user").expect("user turn");
    let user_text = user_msg["content"].as_str().unwrap_or("");
    assert!(!user_text.trim().is_empty(), "user turn must not be empty");
    assert!(user_text.contains("\\int_0^1 x^2\\,dx"), "user turn must contain setup statement: {user_text}");
    srv.shutdown();
    mock.finish();
}
