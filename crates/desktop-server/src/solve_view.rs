//! Pure Solve view model & shared `GraphView` projection (package S4, `00-overview.md` 2.6 & 2.10).
//!
//! Pure function over ledger events: no I/O, no ledger writes, no model calls.
//! Emits `{ setup, run_state, steps, graph, handles, receipts, checks, artifacts, done, toolbox,
//! approval_grants }` with badges computed solely by S2's `badge_for_step` and run state mapped
//! through F9's `run_budget`.

use ledger::Event;
use research_tools::proof_closure::topological_order;
use research_tools::run_budget::{count_model_calls, RunScope, RunState};
use research_tools::solve_model::{
    event_kinds, SolveEntry, SolveRunStatus, StepOrigin,
};
use research_tools::solve_state::SolveState;
use research_tools::solve_status::{
    badge_for_step, compute_all_badges, rests_on_summary, wording, BorderStyle, DisplayGrade,
    StepBadge, Tier,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// On-screen label when a Solve session has been created/configured but no leg has run yet.
/// Kept as a single named constant (`00-overview.md` 3.3, `solve.md` 5.5).
pub const RUN_STATE_READY: &str = "ready";
pub const RUN_STATE_READY_LABEL: &str = "Ready";

/// Legend explanation for exact symbolic checks (`solve.md` 3).
pub const LEGEND_VERIFIED_EXPLANATION: &str =
    "the two sides are equal as exact expressions under the stated assumptions, as decided by SymPy; \
     this does not show that the expression means what the text says";

/// Map S2's [`DisplayGrade`] to the canonical F6 `graph_view.js` / `ui_badge.js` grade key
/// so `graph_view.js` and `ui_badge.js` render solid/dashed borders and tones identically.
pub fn graph_grade_key(grade: DisplayGrade) -> &'static str {
    match grade {
        DisplayGrade::ByCode => "code",
        DisplayGrade::MatchedQuoteFullText => "quote_fulltext",
        DisplayGrade::MatchedQuoteScannedPage => "quote_ocr",
        DisplayGrade::MatchedQuoteAbstract => "quote_abstract",
        DisplayGrade::ModelStatement => "model",
        DisplayGrade::UserStatement => "user",
    }
}

fn badge_icon_and_tone(effective: Tier, stale: bool) -> (&'static str, &'static str) {
    if stale || effective == Tier::Stale {
        return ("warn", "muted");
    }
    match effective {
        Tier::Verified => ("check", "ok"),
        Tier::CheckedNumerically => ("numeric", "ok"),
        Tier::Cited => ("quote", "ok"),
        Tier::Given | Tier::CheckedByYou => ("user", "neutral"),
        Tier::ModelAgreed => ("model", "info"),
        Tier::ModelAgreedSameModel => ("model", "warn"),
        Tier::AssumedByModel => ("warn", "warn"),
        Tier::Unchecked => ("info", "muted"),
        Tier::Failed => ("error", "err"),
        Tier::Stale => ("warn", "muted"),
    }
}

fn border_str(border: BorderStyle) -> &'static str {
    match border {
        BorderStyle::Solid => "solid",
        BorderStyle::Dashed => "dashed",
        BorderStyle::Plain => "plain",
        BorderStyle::None => "none",
    }
}

fn run_state_key(state: RunState) -> &'static str {
    match state {
        RunState::Running => "running",
        RunState::WaitingForYou => "waiting",
        RunState::Paused => "paused",
        RunState::Interrupted => "interrupted",
        RunState::Failed => "failed",
        RunState::Done => "done",
        RunState::DoneWithOpenItems => "done_open_items",
    }
}

fn last_leg_ended_cleanly(events: &[Event]) -> Option<bool> {
    if let Some(idx) = events.iter().rposition(|ev| matches!(ev.kind.as_str(), "session.start" | "session.resume")) {
        return Some(events[idx + 1..].iter().any(|ev| ev.kind == "session.end"));
    }
    events.iter().any(|ev| matches!(ev.kind.as_str(), "model.request" | "tool.request")).then_some(false)
}

fn done_open_items(state: &SolveState) -> Value {
    let Some(done) = &state.done else {
        return json!([]);
    };
    let mut items = Vec::new();
    for c in &done.criteria {
        if !c.met {
            items.push(json!({ "kind": "unmet_criterion", "id": c.id }));
        }
    }
    for eid in &done.rests_on.assumed_by_model {
        items.push(json!({ "kind": "unacknowledged_assumption", "entry_id": eid }));
    }
    Value::Array(items)
}

fn build_run_state_json(
    state: &SolveState,
    events: &[Event],
    running: bool,
    pending_approvals_count: usize,
) -> Value {
    let model_calls_used = count_model_calls(events, RunScope::All);
    let model_calls_max = state.setup.as_ref().map(|s| s.budgets.max_model_calls).unwrap_or(60);

    let (state_key, label, reason, open_items) = if pending_approvals_count > 0 {
        (
            run_state_key(RunState::WaitingForYou),
            RunState::WaitingForYou.label(),
            Some("approval_needed".to_string()),
            json!([]),
        )
    } else if running {
        (run_state_key(RunState::Running), RunState::Running.label(), None, json!([]))
    } else {
        match state.run_status {
            SolveRunStatus::Done(_) => {
                let mapped = state.run_status.to_run_state().unwrap_or(RunState::Done);
                (run_state_key(mapped), mapped.label(), None, done_open_items(state))
            }
            SolveRunStatus::Paused(r) => {
                let mapped = state.run_status.to_run_state().unwrap_or(RunState::Paused);
                let open_items = state
                    .last_paused
                    .as_ref()
                    .map(|p| json!(p.open_items))
                    .unwrap_or_else(|| json!([]));
                (run_state_key(mapped), mapped.label(), Some(r.as_str().to_string()), open_items)
            }
            SolveRunStatus::Running => match last_leg_ended_cleanly(events) {
                Some(true) => (run_state_key(RunState::Paused), RunState::Paused.label(), Some("end_turn".to_string()), json!([])),
                Some(false) => (run_state_key(RunState::Interrupted), RunState::Interrupted.label(), Some("interrupted".to_string()), json!([])),
                None => (RUN_STATE_READY, RUN_STATE_READY_LABEL, None, json!([])),
            },
            SolveRunStatus::Idle => {
                (RUN_STATE_READY, RUN_STATE_READY_LABEL, None, json!([]))
            }
        }
    };

    json!({
        "state": state_key,
        "label": label,
        "reason": reason,
        "running": running,
        "model_calls_used": model_calls_used,
        "model_calls_max": model_calls_max,
        "open_items": open_items,
    })
}

fn latest_ask_before_running(events: &[Event]) -> bool {
    events
        .iter()
        .rev()
        .find(|ev| ev.kind == event_kinds::SOLVE_SETUP)
        .and_then(|ev| ev.payload.get("ask_before_running"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn clip_label(s: &str, max_chars: usize) -> String {
    let trimmed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn build_step_json(
    state: &SolveState,
    entry: &SolveEntry,
    badge: Option<&StepBadge>,
    current_math_sha: Option<&str>,
) -> Value {
    let rests_on = rests_on_summary(state, &entry.entry_id);
    let matching_check_ids: Vec<&str> = state
        .checks
        .iter()
        .filter(|chk| {
            entry.claim_sha256.as_deref() == Some(chk.claim_sha256.as_str())
                && current_math_sha.is_none_or(|m| m == chk.math_sha256.as_str())
        })
        .map(|chk| chk.check_id.as_str())
        .collect();

    let mut cited_calls: Vec<&str> = Vec::new();
    if let Some(c) = entry.tool_call_ref.as_deref() {
        cited_calls.push(c);
    }
    for c in &entry.tool_call_refs {
        if !cited_calls.contains(&c.as_str()) {
            cited_calls.push(c.as_str());
        }
    }
    let receipt_refs: Vec<Value> = cited_calls
        .into_iter()
        .map(|cid| {
            let found = state.receipts.get(cid);
            json!({
                "call_id": cid,
                "present": found.is_some(),
                "ok": found.map(|r| r.ok),
                "missing": found.is_none(),
            })
        })
        .collect();

    json!({
        "entry_id": entry.entry_id,
        "seq": entry.seq,
        "entry_type": entry.entry_type,
        "summary": entry.summary,
        "details": entry.details,
        "depends_on": entry.depends_on,
        "derived_depends_on": entry.derived_depends_on,
        "claims": entry.claims,
        "claim_sha256": entry.claim_sha256,
        "entry_sha256": entry.entry_sha256,
        "origin": entry.origin,
        "source_ref": entry.source_ref,
        "transcription": entry.transcription,
        "superseded_by": entry.superseded_by,
        "badge": badge,
        "rests_on": rests_on,
        "matching_check_ids": matching_check_ids,
        "receipt_refs": receipt_refs,
    })
}

fn build_graph_view(
    state: &SolveState,
    ordered_entries: &[&SolveEntry],
    badges: &BTreeMap<String, StepBadge>,
) -> Value {
    let live_ids: BTreeSet<&str> = ordered_entries.iter().map(|e| e.entry_id.as_str()).collect();
    let mut has_any_verified = false;

    let nodes: Vec<Value> = ordered_entries
        .iter()
        .map(|entry| {
            let badge = badges.get(&entry.entry_id);
            let effective = badge.map(|b| b.effective).unwrap_or(Tier::Unchecked);
            let own = badge.map(|b| b.own).unwrap_or(Tier::Unchecked);
            let stale = badge.is_some_and(|b| b.stale);
            if entry.superseded_by.is_none() && !stale && own == Tier::Verified {
                has_any_verified = true;
            }
            let grade_enum = badge.map(|b| b.grade).unwrap_or(DisplayGrade::ModelStatement);
            let border = badge.map(|b| b.border).unwrap_or(BorderStyle::Dashed);
            let label_text = badge
                .map(|b| b.label.clone())
                .unwrap_or_else(|| wording::BADGE_UNCHECKED.to_string());
            let (icon, tone) = badge_icon_and_tone(effective, stale);

            // `graph_view.js` automatically appends a `Stale` badge whenever `node.stale` is true;
            // avoid duplicating `"Stale"` while keeping `steps[].badge.label` exact from S2.
            let primary_text = if stale && label_text == wording::BADGE_STALE {
                "Needs re-check".to_string()
            } else {
                label_text
            };
            let mut node_badges = vec![json!({
                "text": primary_text,
                "icon": icon,
                "tone": tone,
                "border": border_str(border),
            })];
            if let Some(own_line) = badge.and_then(|b| b.own_check_line.as_deref()) {
                node_badges.push(json!({
                    "text": own_line,
                    "icon": "check",
                    "tone": "muted",
                    "border": "solid",
                }));
            }
            if let Some(tag) = badge.and_then(|b| b.provenance_tag.as_deref()) {
                node_badges.push(json!({
                    "text": tag,
                    "icon": if tag == wording::TAG_OCR { "ocr" } else { "user" },
                    "tone": if tag == wording::TAG_OCR { "warn" } else { "neutral" },
                }));
            }
            let flags = badge.map(|b| b.flags.clone()).unwrap_or_default();
            let summary_clip = clip_label(&entry.summary, 64);

            json!({
                "id": entry.entry_id,
                "kind": entry.entry_type.as_str(),
                "label": format!("{}: {}", entry.entry_id, summary_clip),
                "group": Value::Null,
                "parent": Value::Null,
                "grade": graph_grade_key(grade_enum),
                "display_grade": grade_enum,
                "border": border_str(border),
                "badges": node_badges,
                "flags": flags,
                "stale": stale,
                "dim": entry.superseded_by.is_some(),
            })
        })
        .collect();

    let mut edges: Vec<Value> = Vec::new();
    for entry in ordered_entries {
        let mut seen_src: BTreeSet<&str> = BTreeSet::new();
        for dep in &entry.derived_depends_on {
            if dep != &entry.entry_id && live_ids.contains(dep.as_str()) && seen_src.insert(dep.as_str()) {
                edges.push(json!({
                    "src": dep,
                    "dst": entry.entry_id,
                    "kind": "derived_depends_on",
                    "role": "statement",
                    "basis": "code",
                }));
            }
        }
        let declared_basis = match entry.origin {
            Some(StepOrigin::Setup | StepOrigin::Given | StepOrigin::User) => "code",
            _ => "model",
        };
        for dep in &entry.depends_on {
            if dep != &entry.entry_id && live_ids.contains(dep.as_str()) && seen_src.insert(dep.as_str()) {
                edges.push(json!({
                    "src": dep,
                    "dst": entry.entry_id,
                    "kind": "depends_on",
                    "role": "statement",
                    "basis": declared_basis,
                }));
            }
        }
    }

    let mut legend = Vec::new();
    if has_any_verified {
        legend.push(json!({
            "tier": "verified",
            "label": wording::BADGE_VERIFIED,
            "explanation": LEGEND_VERIFIED_EXPLANATION,
            "icon": "check",
            "tone": "ok",
            "dashed": false,
        }));
    }
    legend.push(json!({
        "tier": "checked_numerically",
        "label": wording::BADGE_CHECKED_NUMERICALLY,
        "icon": "numeric",
        "tone": "ok",
        "dashed": false,
    }));
    legend.push(json!({
        "tier": "cited",
        "label": wording::BADGE_CITED,
        "icon": "quote",
        "tone": "ok",
        "dashed": false,
    }));
    legend.push(json!({
        "tier": "model_agreed",
        "label": format!("{} / {}", wording::BADGE_MODEL_AGREED, wording::BADGE_UNCHECKED),
        "icon": "model",
        "tone": "muted",
        "dashed": true,
    }));
    legend.push(json!({
        "tier": "given",
        "label": format!("{} / {} ({})", wording::BADGE_GIVEN, wording::BADGE_CHECKED_BY_YOU, wording::TAG_YOU),
        "icon": "user",
        "tone": "neutral",
        "dashed": false,
    }));

    let _ = state;
    json!({
        "nodes": nodes,
        "edges": edges,
        "legend": legend,
    })
}

/// Build the complete Solve view JSON payload for `GET /api/sessions/{id}/solve/view`
/// and the `"solve"` field of `GET /api/sessions/{id}`.
pub fn build_solve_view(
    events: &[Event],
    running: bool,
    toolbox: Value,
    approval_grants: Value,
    model_connected: bool,
) -> Value {
    let state = SolveState::from_events(events);
    let badges = compute_all_badges(&state);
    let current_math_sha = state.setup.as_ref().map(|s| s.math_sha256.as_str());

    let ordered_ids = topological_order(&state.entries).unwrap_or_else(|e| e);
    let mut ordered_entries: Vec<&SolveEntry> = Vec::with_capacity(state.entries.len());
    let mut placed = BTreeSet::new();
    for id in &ordered_ids {
        if let Some(entry) = state.entry(id) {
            if placed.insert(entry.entry_id.as_str()) {
                ordered_entries.push(entry);
            }
        }
    }
    for entry in &state.entries {
        if placed.insert(entry.entry_id.as_str()) {
            ordered_entries.push(entry);
        }
    }

    let steps: Vec<Value> = ordered_entries
        .iter()
        .map(|entry| {
            let badge = badge_for_step(&state, &entry.entry_id);
            build_step_json(&state, entry, badge.as_ref(), current_math_sha)
        })
        .collect();

    let graph = build_graph_view(&state, &ordered_entries, &badges);
    let handles: Vec<Value> = state
        .handles
        .iter()
        .map(|(h, expr)| {
            json!({
                "handle": h.to_string(),
                "name": expr.name,
                "version": expr.version,
                "latex": expr.latex,
                "str_preview": expr.str_preview,
                "srepr_sha256": expr.srepr_sha256,
                "free_symbols": expr.free_symbols,
                "produced_by": expr.produced_by,
                "owner_entry": expr.owner_entry,
                "portable": expr.portable,
            })
        })
        .collect();

    let receipts: Vec<Value> = state
        .receipts
        .values()
        .filter_map(|r| serde_json::to_value(r).ok())
        .collect();

    let setup_json = match &state.setup {
        Some(s) => {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("ask_before_running".into(), json!(latest_ask_before_running(events)));
            }
            v
        }
        None => Value::Null,
    };

    let pending_count = approval_grants
        .get("pending_approvals")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let supplied_records =
        research_tools::lessons_prompt::read_supplied_lessons(events, None, None);
    let supplied_count =
        research_tools::lessons_prompt::count_distinct_supplied_notes(&supplied_records);
    let supplied_disclosure =
        research_tools::lessons_prompt::supplied_notes_disclosure(supplied_count);

    json!({
        "setup": setup_json,
        "run_state": build_run_state_json(&state, events, running, pending_count),
        "steps": steps,
        "graph": graph,
        "handles": handles,
        "receipts": receipts,
        "checks": state.checks,
        "artifacts": state.artifacts,
        "done": state.done,
        "result_sheet": crate::cli_turn_solve::build_ledger_result_sheet(&state),
        "plan": state.plan,
        "toolbox": toolbox,
        "approval_grants": approval_grants,
        "model_connected": model_connected,
        "lessons_supplied": {
            "count": supplied_count,
            "disclosure": supplied_disclosure,
            "records": supplied_records,
        },
    })
}

/// Note displayed beside worker-session grants (`solve.md` D5, `00-overview.md` 3.1 row 13).
pub const WORKER_RESTART_NOTE: &str = "A worker restart asks again.";
/// Graceful Linux fallback toolbox line (`solve-s4-s5-setup.md` S4c).
pub const LINUX_TOOLBOX_LINE: &str = "Toolbox: unavailable (Windows sandbox required)";

/// Probe the Python compute toolbox safely without spawning Windows-only `AppContainerSandbox` on Linux.
pub fn probe_toolbox(state: &crate::state::DesktopState) -> Value {
    let configured = state.cfg.python.is_some();
    #[cfg(not(windows))]
    {
        json!({
            "available": false,
            "sandbox": "windows_required",
            "line": LINUX_TOOLBOX_LINE,
            "configured_python": configured,
            "python": Value::Null, "sympy": Value::Null, "numpy": Value::Null, "mpmath": Value::Null,
        })
    }
    #[cfg(windows)]
    {
        json!({
            "available": configured,
            "sandbox": "appcontainer",
            "line": if configured {
                "Toolbox: bundled Python runtime ready (versions reported on worker start)"
            } else {
                "Toolbox: bundled Python runtime not configured"
            },
            "configured_python": configured,
            "python": Value::Null, "sympy": Value::Null, "numpy": Value::Null, "mpmath": Value::Null,
        })
    }
}

/// Describe all grants a Solve run needs (`solve.md` D5, `00-overview.md` 1 & 3.1 row 13).
///
/// Honest pre-turn capability report:
/// - `pre_turn_session_capability_supported: true` (`UiApprover` prompts & binds session capabilities
///   outside a running model turn, verified in `mode_shell.rs` and `solve_setup_routes.rs`).
/// - `pre_turn_worker_session_supported: false` (`WorkerSession` grants in `router.rs` bind per
///   caller/tool/def_hash/policy/live `WorkerEpoch` and require `contained == true`, so they are
///   prompted when the first turn starts a contained worker; zero auto-approve).
pub fn build_solve_approval_grants(state: &crate::state::DesktopState, session_id: &str) -> Value {
    let active_session_grants = crate::session_lifecycle::list_session_grants(state, session_id);
    let pending = state.pending_for_session(session_id);
    let contained = cfg!(windows);
    let net_granted = active_session_grants.contains(&"network-read");
    let exec = policy::EffectClass::ExecuteCode.as_str();
    let net = policy::EffectClass::NetworkRead.as_str();
    let worker_grant = |id: &str, caller: &str, tool_id: &str, label: &str| {
        json!({
            "id": id, "kind": "worker_session", "caller": caller, "tool_id": tool_id,
            "effect": exec, "label": label, "granted": false,
            "restart_asks_again": true, "note": WORKER_RESTART_NOTE,
        })
    };
    let grants = vec![
        worker_grant("solve-worker-solver-sympy", "solver:solve", "sympy_eval",
            "Solver runs Python calculations (sympy_eval)"),
        worker_grant("solve-worker-host-check", "host:check", "check_run",
            "Host runs exact and numeric checks (check_run)"),
        worker_grant("solve-worker-verifier-sympy", "verifier:solve", "sympy_eval",
            "Verifier runs isolated Python calculations (sympy_eval)"),
        worker_grant("solve-worker-verifier-check", "verifier:solve", "check_run",
            "Verifier runs registered checks (check_run)"),
        json!({
            "id": "solve-session-network-read", "kind": "session_capability",
            "effect": net, "label": "Look up cited papers on literature hosts (optional)",
            "granted": net_granted,
            "can_grant_for_session": crate::session_lifecycle::can_grant_for_session(policy::EffectClass::NetworkRead, contained),
            "restart_asks_again": false, "optional": true,
        }),
    ];
    json!({
        "pre_turn_approver_supported": false,
        "pre_turn_session_capability_supported": true,
        "pre_turn_worker_session_supported": false,
        "worker_restart_note": WORKER_RESTART_NOTE,
        "active_session_grants": active_session_grants,
        "pending_approvals": pending,
        "grants": grants,
    })
}
