//! Sign in with ChatGPT through the desktop server, against a loopback mock of OpenAI's
//! auth and API hosts (no real network, no browser: the opener is replaced and the
//! callback is called directly). Windows only: tokens are stored with DPAPI.
#![cfg(windows)]

mod common;

use chatgpt_auth::testkit::{browser_get, id_claims, jwks, sign_jwt, url_param, MockAuth, Recorded, Reply};
use common::{launch_cookie, raw_http, test_cfg};
use desktop_server::chatgpt::ChatGptTestHooks;
use desktop_server::start_desktop_server;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

const SECRETS: &[&str] = &["acc-1-SECRETVALUE", "rt-1-SECRETVALUE", "code-xyz"];

struct Mock {
    nonce: Mutex<String>,
    issuer: Mutex<String>,
    streams: Mutex<VecDeque<Vec<u8>>>,
}

fn sse(events: &[Value]) -> Vec<u8> {
    events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e)).collect::<String>().into_bytes()
}

fn completed(id: &str, item: Value) -> Value {
    json!({ "type": "response.completed", "response": { "id": id, "status": "completed", "model": "gpt-test-sol",
        "output": [item], "usage": { "input_tokens": 5, "output_tokens": 5 } } })
}

fn reply(m: &Mock, r: &Recorded) -> Reply {
    match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/api/accounts/oauth/token") => {
            let client = r.form("client_id").unwrap_or_default();
            let exp = chatgpt_auth::codec::now_s() + 3600;
            let id = sign_jwt(&id_claims(&m.issuer.lock().unwrap(), &client, &m.nonce.lock().unwrap(), "sub-9", exp));
            Reply::json(200, &json!({ "access_token": SECRETS[0], "refresh_token": SECRETS[1], "id_token": id,
                "token_type": "Bearer", "expires_in": 3600,
                "scope": "chatgpt.tokens.use.direct email offline_access openid profile resource.invoke" }))
        }
        ("GET", "/.well-known/jwks.json") => Reply::json(200, &jwks()),
        ("POST", "/api/accounts/oauth/revoke") => Reply { status: 200, content_type: "application/json", body: Vec::new() },
        ("GET", "/v1/models") => Reply::json(200, &json!({ "models": [
            { "slug": "gpt-test-sol", "display_name": "GPT Test", "visibility": "list" },
            { "slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide" } ] })),
        ("POST", "/v1/responses") => {
            let parsed: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
            if parsed["input"].as_array().is_none_or(Vec::is_empty) {
                return Reply::json(400, &json!({
                    "error": {
                        "code": "missing_required_parameter",
                        "param": "input",
                        "message": "One of \"input\" or \"previous_response_id\" or 'prompt' or 'conversation' must be provided."
                    }
                }));
            }
            match m.streams.lock().unwrap().pop_front() {
                Some(body) => Reply { status: 200, content_type: "text/event-stream", body },
                None => Reply::json(500, &json!({ "error": { "code": "exhausted" } })),
            }
        }
        _ => Reply::json(404, &json!({})),
    }
}

fn scan_dir_for_secrets(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            scan_dir_for_secrets(&p);
        } else if let Ok(bytes) = std::fs::read(&p) {
            for s in SECRETS {
                assert!(!bytes.windows(s.len()).any(|w| w == s.as_bytes()), "{s} in {}", p.display());
            }
        }
    }
}

fn assert_clean(label: &str, text: &str) {
    for s in SECRETS {
        assert!(!text.contains(s), "{label} contains secret material: {text}");
    }
}

#[test]
fn sign_in_pick_a_model_run_a_turn_with_approval_and_sign_out() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let state = Arc::new(Mock { nonce: Mutex::new(String::new()), issuer: Mutex::new(String::new()), streams: Mutex::new(VecDeque::new()) });
    let call = json!({ "type": "function_call", "call_id": "call_w1", "namespace": "research", "name": "fs_write",
        "arguments": "{\"path\":\"notes.tex\",\"content\":\"Hello\\n\"}" });
    let text = json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Wrote notes.tex." }] });
    state.streams.lock().unwrap().extend([
        sse(&[json!({ "type": "response.output_item.done", "item": call }), completed("resp_1", call.clone())]),
        sse(&[json!({ "type": "response.output_text.delta", "delta": "Wrote" }), completed("resp_2", text.clone())]),
    ]);
    let s2 = Arc::clone(&state);
    let mock = MockAuth::start(Arc::new(move |r: &Recorded| reply(&s2, r)));
    *state.issuer.lock().unwrap() = mock.base.clone();

    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let o2 = Arc::clone(&opened);
    // A launch --api-base must not redirect the ChatGPT route (it points nowhere useful).
    let mut cfg = test_cfg(&data, Some(ws.clone()), Some("http://127.0.0.1:9".into()), None);
    cfg.chatgpt_test = Some(ChatGptTestHooks {
        mock_base: mock.base.clone(),
        opener: Arc::new(move |u: &str| {
            o2.lock().unwrap().push(u.to_string());
            Ok(())
        }),
        login_timeout: Duration::from_secs(60),
    });
    let srv = start_desktop_server(cfg).unwrap();
    let port = srv.addr.port();
    let tok = srv.info.token.clone();
    let h = [("X-ReSearch-Token", tok.as_str()), ("Content-Type", "application/json")];
    let mut seen = String::new();
    let mut call_api = |method: &str, path: &str, body: Value| -> (u16, Value) {
        let raw = if body.is_null() { Vec::new() } else { serde_json::to_vec(&body).unwrap() };
        let (st, _, b) = raw_http(port, method, path, &h, &raw);
        let text = String::from_utf8_lossy(&b).into_owned();
        seen.push_str(&text);
        (st, serde_json::from_str(&text).unwrap_or(Value::Null))
    };
    let chatgpt = |conns: &Value| conns["connections"].as_array().unwrap().iter().find(|c| c["id"] == "chatgpt").unwrap().clone();

    // Every route needs the launch token.
    for (m, p) in [("POST", "/api/connections/chatgpt/login"), ("POST", "/api/connections/chatgpt/open"), ("POST", "/api/connections/chatgpt/logout"), ("GET", "/api/connections/chatgpt/login")] {
        let (st, _, _) = raw_http(port, m, p, &[], b"");
        assert!(matches!(st, 401 | 403), "{m} {p} without token: {st}");
    }

    let (_, conns) = call_api("GET", "/api/connections", Value::Null);
    let c = chatgpt(&conns);
    assert_eq!((c["connected"].clone(), c["action"].clone(), c["kind"].clone()), (json!(false), json!("signin"), json!("subscription")));
    assert_eq!(c["session_note"], json!("runs on your ChatGPT plan"));
    assert_eq!(call_api("POST", "/api/connections/chatgpt/open", Value::Null).0, 409, "nothing to open yet");

    // Sign in: the server returns the URL; "Open in browser" opens exactly that URL.
    let (st, login) = call_api("POST", "/api/connections/chatgpt/login", Value::Null);
    assert_eq!(st, 200, "{login}");
    let url = login["authorize_url"].as_str().unwrap().to_string();
    assert!(url.starts_with(&format!("{}/api/accounts/authorize?", mock.base)));
    let (_, conns) = call_api("GET", "/api/connections", Value::Null);
    assert_eq!(chatgpt(&conns)["pending_authorize_url"], json!(url));
    assert_eq!(call_api("POST", "/api/connections/chatgpt/open", Value::Null).0, 200);
    assert_eq!(*opened.lock().unwrap(), vec![url.clone()]);
    assert_eq!(call_api("GET", "/api/connections/chatgpt/login", Value::Null).1["state"], json!("pending"));

    // The browser comes back to the loopback callback.
    *state.nonce.lock().unwrap() = url_param(&url, "nonce").unwrap();
    let redirect = url_param(&url, "redirect_uri").unwrap();
    let cb_port: u16 = redirect.trim_start_matches("http://127.0.0.1:").split('/').next().unwrap().parse().unwrap();
    let st_param = url_param(&url, "state").unwrap();
    let (cb_status, cb_page) = browser_get(cb_port, &format!("/auth/callback?code={}&state={st_param}&client_id=oaiapp_desk", SECRETS[2])).unwrap();
    assert_eq!(cb_status, 200, "{cb_page}");
    assert_clean("callback page", &cb_page);
    assert_eq!(call_api("GET", "/api/connections/chatgpt/login", Value::Null).1["state"], json!("succeeded"));
    let (_, conns) = call_api("GET", "/api/connections", Value::Null);
    let c = chatgpt(&conns);
    assert_eq!((c["connected"].clone(), c["action"].clone(), c["can_signout"].clone(), c["plan_enabled"].clone()), (json!(true), json!("signout"), json!(true), json!(true)), "{c}");

    // Models: from /v1/models with the token, only `visibility == "list"`.
    let (_, models) = call_api("GET", "/api/models?refresh=1", Value::Null);
    let g = models["providers"].as_array().unwrap().iter().find(|g| g["id"] == "chatgpt").expect("chatgpt group").clone();
    assert_eq!(g["route"], json!("api"));
    let ids: Vec<&str> = g["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["gpt-test-sol"]);
    let list_req = mock.requests().into_iter().find(|r| r.path == "/v1/models").unwrap();
    assert_eq!(list_req.header("authorization"), Some(format!("Bearer {}", SECRETS[0]).as_str()));

    // A key, address, non-string value, or chatgpt role base_url is refused without touching providers.json.
    let providers_path = data.join("providers.json");
    let before = std::fs::read(&providers_path).ok();
    for bad in [
        json!({ "keys": { "chatgpt": "sk-x" } }),
        json!({ "keys": { "chatgpt": 123 } }),
        json!({ "base_urls": { "chatgpt": "https://evil.example" } }),
        json!({ "base_urls": { "chatgpt": true } }),
        json!({ "roles": {
            "solver": { "provider": "chatgpt", "model": "gpt-test-sol", "base_url": "https://evil.example" },
            "step_verifier": { "provider": "openai", "model": "gpt-4.1-mini" },
            "strategist": { "provider": "openai", "model": "gpt-4.1-mini" },
            "narrator": { "provider": "openai", "model": "gpt-4.1-mini" }
        }}),
    ] {
        assert_eq!(call_api("PUT", "/api/settings/providers", bad).0, 400);
        assert_eq!(std::fs::read(&providers_path).ok(), before);
    }

    // A session on the ChatGPT plan: in-core loop, approval card answered over the API.
    common::disable_auto_approve(port, &tok);
    let (_, created) = call_api("POST", "/api/sessions", json!({ "title": "ChatGPT", "workspace_root": ws, "provider": "chatgpt" }));
    let sid = created["id"].as_str().unwrap().to_string();
    let cookie = launch_cookie(port, &tok);
    let mut sse_stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    sse_stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(sse_stream, "GET /api/sessions/{sid}/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {cookie}\r\n\r\n").unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(sse_stream).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let (st, _) = call_api("POST", &format!("/api/sessions/{sid}/messages"), json!({ "prompt": "Write notes.tex", "provider": "chatgpt", "model": "gpt-test-sol" }));
    assert!(matches!(st, 200 | 202));
    let mut frames = String::new();
    let mut approval: Option<Value> = None;
    let mut done = false;
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        frames.push_str(&line);
        frames.push('\n');
        if let Some(d) = line.strip_prefix("data:") {
            let v: Value = serde_json::from_str(d.trim()).unwrap_or(Value::Null);
            if approval.is_none() && v.get("digest").is_some() && v.get("call_id").is_some() && v.get("edit_preview").is_some() {
                approval = Some(v.clone());
                let path = format!("/api/sessions/{sid}/approvals/{}", v["call_id"].as_str().unwrap());
                assert_eq!(call_api("POST", &path, json!({ "digest": v["digest"], "approved": true })).0, 200);
            }
        }
        if line.starts_with("event: turn_complete") || line.starts_with("event:turn_complete") {
            done = true;
            break;
        }
        if line.contains("turn_error") {
            panic!("turn failed: {frames}");
        }
    }
    assert!(approval.is_some() && done, "approval and completion expected: {frames}");
    assert_eq!(std::fs::read_to_string(ws.join("notes.tex")).unwrap(), "Hello\n");
    let bodies: Vec<Value> = mock.requests().iter().filter(|r| r.path == "/v1/responses").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.iter().all(|b| b["store"] == json!(false) && b["stream"] == json!(true)));
    assert_clean("SSE", &frames);

    // Provenance records the route and model, not the token.
    let ledger = ledger::Ledger::open_existing(&data).unwrap();
    let events = ledger.read_session(&sid).unwrap();
    assert_eq!(events[0].payload["provider"], json!("chatgpt-responses"));
    assert_eq!(events[0].payload["model"], json!("gpt-test-sol"));
    assert_eq!(events[0].payload["endpoint"], json!(format!("{}/v1/responses", mock.base)));
    drop(ledger);

    // Sign out: revoke, delete, disconnect; the group leaves the model list.
    let (st, out) = call_api("POST", "/api/connections/chatgpt/logout", Value::Null);
    assert_eq!((st, out["revoked"].clone()), (200, json!(true)), "{out}");
    let rv = mock.requests().into_iter().find(|r| r.path == "/api/accounts/oauth/revoke").unwrap();
    assert_eq!(rv.form("token").as_deref(), Some(SECRETS[1]));
    let (_, conns) = call_api("GET", "/api/connections", Value::Null);
    assert_eq!(chatgpt(&conns)["connected"], json!(false));
    let (_, models) = call_api("GET", "/api/models", Value::Null);
    assert!(!models["providers"].as_array().unwrap().iter().any(|g| g["id"] == "chatgpt"));

    drop(call_api);
    assert_clean("API responses", &seen);
    srv.shutdown();
    let _ = reader.join();
    scan_dir_for_secrets(&data);
}

#[test]
fn chatgpt_turn_refused_without_signin_or_plan_scope_and_ignores_generic_key() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let nonce = Arc::new(Mutex::new(String::new()));
    let issuer = Arc::new(Mutex::new(String::new()));
    let (n2, i2) = (Arc::clone(&nonce), Arc::clone(&issuer));
    let mock = MockAuth::start(Arc::new(move |r: &Recorded| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/api/accounts/oauth/token") => {
            let client = r.form("client_id").unwrap_or_default();
            let exp = chatgpt_auth::codec::now_s() + 3600;
            let id = sign_jwt(&id_claims(&i2.lock().unwrap(), &client, &n2.lock().unwrap(), "sub-no-plan", exp));
            // Deliberately omits chatgpt.tokens.use.direct -> plan_enabled == false.
            Reply::json(200, &json!({ "access_token": SECRETS[0], "refresh_token": SECRETS[1], "id_token": id,
                "token_type": "Bearer", "expires_in": 3600, "scope": "email offline_access openid profile" }))
        }
        ("GET", "/.well-known/jwks.json") => Reply::json(200, &jwks()),
        ("POST", "/api/accounts/oauth/revoke") => Reply { status: 200, content_type: "application/json", body: Vec::new() },
        _ => Reply::json(404, &json!({})),
    }));
    *issuer.lock().unwrap() = mock.base.clone();

    let mut cfg = test_cfg(&data, None, None, None);
    cfg.connection_env = Some(std::collections::BTreeMap::from([
        ("RE_SEARCH_PROVIDER".to_string(), "chatgpt".to_string()),
        ("RE_SEARCH_API_KEY".to_string(), "sk-generic-must-be-ignored".to_string()),
    ]));
    cfg.chatgpt_test = Some(ChatGptTestHooks {
        mock_base: mock.base.clone(),
        opener: Arc::new(|_| Ok(())),
        login_timeout: Duration::from_secs(60),
    });
    let srv = start_desktop_server(cfg).unwrap();
    let port = srv.addr.port();
    let tok = srv.info.token.clone();
    let h = [("X-ReSearch-Token", tok.as_str()), ("Content-Type", "application/json")];
    let call_api = |method: &str, path: &str, body: Value| -> (u16, Value) {
        let raw = if body.is_null() { Vec::new() } else { serde_json::to_vec(&body).unwrap() };
        let (st, _, b) = raw_http(port, method, path, &h, &raw);
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    };

    let wait_turn_error = |sid: &str| -> String {
        let cookie = launch_cookie(port, &tok);
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        write!(s, "GET /api/sessions/{sid}/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {cookie}\r\n\r\n").unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        assert!(matches!(call_api("POST", &format!("/api/sessions/{sid}/messages"), json!({ "prompt": "hi", "provider": "chatgpt" })).0, 200 | 202));
        let mut buf = String::new();
        while let Ok(line) = rx.recv_timeout(Duration::from_secs(15)) {
            buf.push_str(&line);
            buf.push('\n');
            if line.contains("turn_error") {
                return buf;
            }
            assert!(!line.contains("turn_complete"), "turn must not complete: {buf}");
        }
        panic!("expected turn_error, got: {buf}");
    };

    // 1. Not signed in: turn is refused (RE_SEARCH_API_KEY is ignored).
    let sid = call_api("POST", "/api/sessions", json!({ "title": "no-signin" })).1["id"].as_str().unwrap().to_string();
    let _ = wait_turn_error(&sid);
    assert!(!mock.requests().iter().any(|r| r.path == "/v1/responses"));

    // 2. Sign in without plan scope -> signed_in=true, plan_enabled=false.
    let url = call_api("POST", "/api/connections/chatgpt/login", Value::Null).1["authorize_url"].as_str().unwrap().to_string();
    *nonce.lock().unwrap() = url_param(&url, "nonce").unwrap();
    let redirect = url_param(&url, "redirect_uri").unwrap();
    let cb_port: u16 = redirect.trim_start_matches("http://127.0.0.1:").split('/').next().unwrap().parse().unwrap();
    let st_param = url_param(&url, "state").unwrap();
    assert_eq!(browser_get(cb_port, &format!("/auth/callback?code={}&state={st_param}&client_id=oaiapp_desk", SECRETS[2])).unwrap().0, 200);

    let (_, conns) = call_api("GET", "/api/connections", Value::Null);
    let c = conns["connections"].as_array().unwrap().iter().find(|c| c["id"] == "chatgpt").unwrap();
    assert_eq!((c["connected"].clone(), c["plan_enabled"].clone(), c["can_signout"].clone()), (json!(false), json!(false), json!(true)), "{c}");
    assert_eq!((c["action"].clone(), c["action_label"].clone()), (json!("signin"), json!("Allow ChatGPT plan")), "{c}");
    let (_, models) = call_api("GET", "/api/models", Value::Null);
    assert!(!models["providers"].as_array().unwrap().iter().any(|g| g["id"] == "chatgpt"));

    let _ = wait_turn_error(&sid);
    assert!(!mock.requests().iter().any(|r| r.path == "/v1/responses"));
    assert_eq!(call_api("POST", "/api/connections/chatgpt/logout", Value::Null).0, 200);
    srv.shutdown();
}

#[test]
fn solve_first_turn_on_chatgpt_responses_route_with_not_selected_solver_and_empty_continue_succeeds() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    let state = Arc::new(Mock {
        nonce: Mutex::new(String::new()),
        issuer: Mutex::new(String::new()),
        streams: Mutex::new(VecDeque::new()),
    });
    let text = json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": "The integral evaluates to 1/3." }]
    });
    state.streams.lock().unwrap().push_back(sse(&[completed("resp_solve_1", text)]));
    let s2 = Arc::clone(&state);
    let mock = MockAuth::start(Arc::new(move |r: &Recorded| reply(&s2, r)));
    *state.issuer.lock().unwrap() = mock.base.clone();

    let mut cfg = test_cfg(&data, None, Some("http://127.0.0.1:9".into()), None);
    cfg.chatgpt_test = Some(ChatGptTestHooks {
        mock_base: mock.base.clone(),
        opener: Arc::new(|_| Ok(())),
        login_timeout: Duration::from_secs(60),
    });
    let srv = start_desktop_server(cfg).unwrap();
    let port = srv.addr.port();
    let tok = srv.info.token.clone();
    let h = [("X-ReSearch-Token", tok.as_str()), ("Content-Type", "application/json")];
    let call_api = |method: &str, path: &str, body: Value| -> (u16, Value) {
        let raw = if body.is_null() { Vec::new() } else { serde_json::to_vec(&body).unwrap() };
        let (st, _, b) = raw_http(port, method, path, &h, &raw);
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    };

    // Sign in with ChatGPT ONLY (no API keys, no default_provider in ui_settings).
    let url = call_api("POST", "/api/connections/chatgpt/login", Value::Null).1["authorize_url"]
        .as_str()
        .unwrap()
        .to_string();
    *state.nonce.lock().unwrap() = url_param(&url, "nonce").unwrap();
    let redirect = url_param(&url, "redirect_uri").unwrap();
    let cb_port: u16 = redirect
        .trim_start_matches("http://127.0.0.1:")
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let st_param = url_param(&url, "state").unwrap();
    assert_eq!(
        browser_get(cb_port, &format!("/auth/callback?code={}&state={st_param}&client_id=oaiapp_desk", SECRETS[2]))
            .unwrap()
            .0,
        200
    );

    // Exact v0.2.0 smoke repro: new Solve session, Solver = Not selected, Verifier = chatgpt / gpt-6.2-sol,
    // statement = "Compute the integral $\int_0^1 x^2\,dx$.", then `POST .../solve/continue` with `{}`.
    let (st, created) = call_api("POST", "/api/sessions", json!({ "mode": "solve" }));
    assert_eq!(st, 200, "{created}");
    let sid = created["id"].as_str().unwrap().to_string();

    let (st, setup_res) = call_api(
        "POST",
        &format!("/api/sessions/{sid}/solve/setup"),
        json!({
            "statement": "Compute the integral $\\int_0^1 x^2\\,dx$.",
            "task_kind": "calculate",
            "target": { "description": "Final calculated expression", "form": "closed_form" },
            "done": [{ "id": "no_open_flags", "kind": "no_open_flags", "params": {} }],
            "conventions": "",
            "symbols": [{ "name": "x", "assumptions": ["real"] }],
            "dimensions": {},
            "assumptions": [],
            "roles": {
                "solver": { "provider": "", "model": "" },
                "verifier": { "provider": "chatgpt", "model": "gpt-6.2-sol" }
            },
            "ask_before_running": false
        }),
    );
    assert_eq!(st, 200, "{setup_res}");

    let (st, cont_res) = call_api("POST", &format!("/api/sessions/{sid}/solve/continue"), json!({}));
    assert_eq!(st, 202, "{cont_res}");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let ledger = ledger::Ledger::open(&data).unwrap();
    while std::time::Instant::now() < deadline {
        let evs = ledger.read_session(&sid).unwrap();
        if evs.iter().any(|e| e.kind == "session.end" || e.kind == "solve.paused") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let evs = ledger.read_session(&sid).unwrap();
    assert!(evs.iter().any(|e| e.kind == "model.response"), "expected model.response, got {evs:#?}");
    assert!(!evs.iter().any(|e| e.kind == "model.error"), "must not emit model.error: {evs:#?}");
    let (_, view) = call_api("GET", &format!("/api/sessions/{sid}/solve/view"), Value::Null);
    assert!(view["run_state"]["model_calls_used"].as_u64().unwrap_or(0) >= 1, "{view}");
    assert_ne!(view["run_state"]["state"], json!("interrupted"), "{view}");

    let resp_req = mock.requests().into_iter().find(|r| r.path == "/v1/responses").expect("/v1/responses request");
    let wire: Value = serde_json::from_str(&resp_req.body).unwrap();
    let input = wire["input"].as_array().expect("input array");
    assert!(!input.is_empty());
    assert!(input[0]["content"].as_str().unwrap_or("").contains("\\int_0^1 x^2\\,dx"));

    assert_eq!(call_api("POST", "/api/connections/chatgpt/logout", Value::Null).0, 200);
    srv.shutdown();
}

