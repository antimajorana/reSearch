//! OpenAI Responses API transport (`POST /v1/responses`) for the ChatGPT-plan route
//! ("Sign in with ChatGPT" for open-source apps).
//!
//! Wire rules from OpenAI's Sign in with ChatGPT documentation ("Models and inference",
//! "Preview limitations"): every request sets `store: false` and `stream: true`, sends the
//! whole history in `input` (no `previous_response_id`), uses `instructions` for the
//! system prompt (explicit `system` message items are rejected), groups function tools in
//! a `namespace` tool, and omits `background`, `conversation`, `max_output_tokens`,
//! `max_tool_calls`, `metadata`, `moderation`, `multi_agent`, `prompt`,
//! `prompt_cache_retention`, `safety_identifier`, `temperature`, `top_logprobs`, `top_p`,
//! `truncation` and `user`. A turn succeeds only on a `response.completed` event; a
//! stream that ends without it is an error.
//!
//! The access token comes from a [`BearerSource`] on every request (so a refreshed token
//! is used as soon as it exists) and is sent only in the `Authorization` header to
//! `https://api.openai.com` (or a plaintext loopback mock in tests). It is scrubbed from
//! every byte the transport returns.

use crate::http::{
    build_ureq_agent, config_err, malformed_response_err, post_json, scrub_bytes, scrub_text, validate_base_url,
    SecretKey, ValidatedBaseUrl,
};
use crate::stream_http::{post_sse_streaming, SseFlavor};
use crate::transport::{
    ContentBlock, Message, ModelRequest, ModelResponse, PreparedRequest, ProviderKind, Role, StopReason,
    StreamObserver, ToolSpec, Transport, TransportError, TransportErrorKind, TransportInfo, DEFAULT_CHATGPT_BASE,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// Name of the namespace tool that holds re:Search's function tools.
pub const TOOL_NAMESPACE: &str = "research";
/// What `TransportInfo::api_key_env` records for this route (a label, never a secret).
pub const CREDENTIAL_LABEL: &str = "chatgpt-sign-in";

/// Supplies a current OAuth access token (refreshing it when needed). Errors carry no
/// token material.
pub trait BearerSource: Send + Sync {
    fn bearer(&self) -> Result<SecretKey, TransportError>;

    /// Forces a token refresh after an unexpected `401` (for example, server-side
    /// revocation before local expiry). Default refuses retry.
    fn force_refresh(&self, _failed_token: &str) -> Result<SecretKey, TransportError> {
        Err(TransportError {
            kind: TransportErrorKind::Auth,
            status: Some(401),
            message: "authentication failed".into(),
            raw_body: None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChatGptConfig {
    /// `https://api.openai.com` in production; a `http://127.0.0.1:<port>` mock in tests.
    pub base_url: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for ChatGptConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_CHATGPT_BASE.to_string(),
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(600),
        }
    }
}

/// The ChatGPT-plan token may be sent only to OpenAI's API host over https, or to a
/// plaintext IPv4 loopback server (tests), and only to `/`, `/v1`, or `/v1/responses`.
pub(crate) fn chatgpt_base_allowed(v: &ValidatedBaseUrl) -> bool {
    if !matches!(v.base_path.as_str(), "" | "/v1" | "/v1/responses") {
        return false;
    }
    if v.is_https {
        v.authority.eq_ignore_ascii_case("api.openai.com")
    } else {
        v.is_loopback_http
            && v.authority
                .strip_prefix("127.0.0.1:")
                .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
    }
}

pub struct ChatGptTransport {
    endpoint: String,
    source: Arc<dyn BearerSource>,
    agent: ureq::Agent,
}

impl ChatGptTransport {
    pub fn new(config: ChatGptConfig, source: Arc<dyn BearerSource>) -> Result<Self, TransportError> {
        let v = validate_base_url(&config.base_url)?;
        if !chatgpt_base_allowed(&v) {
            return Err(config_err(
                "the ChatGPT plan route only talks to https://api.openai.com (or a loopback test server)",
            ));
        }
        let base = v.base_path.strip_suffix("/v1/responses").or_else(|| v.base_path.strip_suffix("/v1")).unwrap_or(&v.base_path);
        let endpoint = format!("{}://{}{}/v1/responses", v.scheme, v.authority, base);
        let agent = build_ureq_agent(v.is_https, config.connect_timeout, config.request_timeout);
        Ok(Self { endpoint, source, agent })
    }

    fn post_with_key(
        &self,
        body: &[u8],
        key: &SecretKey,
        observer: Option<&mut dyn StreamObserver>,
    ) -> Result<Vec<u8>, TransportError> {
        let secret = key.as_str();
        let mut auth = format!("Bearer {secret}");
        let res = {
            let headers = [("Authorization", auth.as_str()), ("Accept", "text/event-stream")];
            match observer {
                Some(obs) => post_sse_streaming(&self.agent, &self.endpoint, &headers, body, secret, SseFlavor::Responses, obs),
                None => post_json(&self.agent, &self.endpoint, &headers, body, secret),
            }
        };
        zeroize::Zeroize::zeroize(&mut auth);
        res
    }

    fn send_inner(
        &self,
        prepared: &PreparedRequest,
        mut observer: Option<&mut dyn StreamObserver>,
    ) -> Result<ModelResponse, TransportError> {
        let key = self.source.bearer()?;
        let secret = key.as_str();
        match self.post_with_key(&prepared.body, &key, match observer.as_deref_mut() {
            Some(o) => Some(&mut *o),
            None => None,
        }) {
            Ok(raw) => parse_responses_stream(raw, secret),
            Err(err) if err.kind == TransportErrorKind::Cancelled => Err(err),
            Err(err) => {
                let described = describe_http_error(err, secret);
                if described.status == Some(401) && !is_user_not_eligible(&described) {
                    if let Ok(fresh) = self.source.force_refresh(secret) {
                        if fresh.as_str() != secret {
                            let fresh_secret = fresh.as_str();
                            let raw = self.post_with_key(&prepared.body, &fresh, observer).map_err(|e2| {
                                if e2.kind == TransportErrorKind::Cancelled { e2 } else { describe_http_error(e2, fresh_secret) }
                            })?;
                            return parse_responses_stream(raw, fresh_secret);
                        }
                    }
                }
                Err(described)
            }
        }
    }
}

fn is_user_not_eligible(err: &TransportError) -> bool {
    err.raw_body
        .as_deref()
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|v| v["error"]["code"].as_str().map(|c| c == "subscription_sharing_user_not_eligible"))
        .unwrap_or(false)
}

impl Transport for ChatGptTransport {
    fn info(&self) -> TransportInfo {
        TransportInfo {
            provider: ProviderKind::ChatGpt.wire_id(),
            endpoint: self.endpoint.clone(),
            api_key_env: CREDENTIAL_LABEL.to_string(),
        }
    }

    fn prepare_request(&self, req: &ModelRequest) -> Result<PreparedRequest, TransportError> {
        Ok(PreparedRequest { body: serialize_responses_request(req)? })
    }

    fn send_prepared(&self, prepared: &PreparedRequest) -> Result<ModelResponse, TransportError> {
        self.send_inner(prepared, None)
    }

    fn send_prepared_streaming(
        &self,
        prepared: &PreparedRequest,
        observer: &mut dyn StreamObserver,
    ) -> Result<ModelResponse, TransportError> {
        self.send_inner(prepared, Some(observer))
    }
}

/// Serialize a request body. `max_tokens` is not sent: `max_output_tokens` is unsupported
/// on this route.
pub fn serialize_responses_request(req: &ModelRequest) -> Result<Vec<u8>, TransportError> {
    if req.model.trim().is_empty() {
        return Err(config_err("model name is empty"));
    }
    let mut obj = Map::new();
    obj.insert("model".into(), json!(req.model));
    if let Some(sys) = req.system.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("instructions".into(), json!(sys));
    }
    let mut input = Vec::new();
    for msg in &req.messages {
        encode_message(msg, &mut input)?;
    }
    if input.is_empty() {
        return Err(config_err("request has no input messages"));
    }
    obj.insert("input".into(), Value::Array(input));
    if !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            json!([{
                "type": "namespace",
                "name": TOOL_NAMESPACE,
                "description": "re:Search research tools. Every call is checked by re:Search's policy and may need the user's approval.",
                "tools": req.tools.iter().map(encode_tool).collect::<Vec<_>>(),
            }]),
        );
    }
    if let Some(effort) = crate::transport::normalize_reasoning_effort(req.reasoning_effort.as_deref()) {
        let mapped = if effort == "max" { "xhigh" } else { effort.as_str() };
        obj.insert("reasoning".into(), json!({ "effort": mapped }));
    }
    obj.insert("store".into(), json!(false));
    obj.insert("stream".into(), json!(true));
    serde_json::to_vec(&Value::Object(obj)).map_err(|e| config_err(format!("failed to serialize request: {e}")))
}

fn normalize_schema(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len() + 1);
            for (k, val) in map {
                out.insert(k.clone(), normalize_schema(val));
            }
            if out.get("type").and_then(Value::as_str) == Some("array") && !out.contains_key("items") {
                out.insert("items".into(), json!({ "type": "string" }));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_schema).collect()),
        other => other.clone(),
    }
}

fn encode_tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "strict": false,
        "parameters": normalize_schema(&spec.input_schema),
    })
}

fn encode_message(msg: &Message, out: &mut Vec<Value>) -> Result<(), TransportError> {
    match msg.role {
        Role::User => {
            let mut text = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::ToolResult { tool_use_id, content, .. } => out.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    })),
                    ContentBlock::Text { text: t } if !t.trim().is_empty() => text.push(t.as_str()),
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text.join("\n") }));
            }
        }
        Role::Assistant => {
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } if !text.is_empty() => {
                        out.push(json!({ "role": "assistant", "content": text }));
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        let arguments = serde_json::to_string(input)
                            .map_err(|e| config_err(format!("failed to encode tool input: {e}")))?;
                        out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "namespace": TOOL_NAMESPACE,
                            "arguments": arguments,
                        }));
                    }
                    // Reasoning items are not replayed: with `store: false` they cannot be
                    // referenced, and the history stays valid without them.
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// One server-sent event: its `event:` name (if any) and its joined `data:` lines.
/// Follows WHATWG SSE: strips a leading UTF-8 BOM and dispatches only events terminated
/// by a blank line.
fn sse_events(text: &str) -> Vec<(Option<String>, String)> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut out = Vec::new();
    let (mut name, mut data): (Option<String>, Vec<&str>) = (None, Vec::new());
    for line in text.split_terminator('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        if line.is_empty() {
            if !data.is_empty() {
                out.push((name.take(), data.join("\n")));
            }
            name = None;
            data.clear();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        }
    }
    out
}

/// Error codes the documentation gives a specific recovery for ("Errors and recovery").
fn recovery_hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "a ChatGPT plan usage limit was reached (it may be a limit for this app); see ChatGPT Settings > Usage (chatgpt.com/settings/usage)"
        }
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "ChatGPT plan usage could not be checked right now; try again later"
        }
        "subscription_sharing_user_not_eligible" => "ChatGPT plan usage is not available for this user, workspace or policy",
        "subscription_sharing_unsupported_capability" => "the request used an input, tool, feature or model this route does not support",
        "subscription_sharing_route_not_supported" => "this endpoint is not supported for ChatGPT plan usage",
        "subscription_sharing_invalid_user" => "the ChatGPT sign-in could not be validated; sign in again",
        "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
            "the sign-in does not authorize this operation; sign in again and allow ChatGPT plan use"
        }
        _ => return None,
    })
}

/// Short, printable, bounded copy of a server-provided code or parameter name.
fn clip_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '[' | ']')).take(80).collect()
}

fn clip_field(v: &Value) -> Option<String> {
    v.as_str().map(clip_code).filter(|s| !s.is_empty())
}

fn kind_for_code(code: &str) -> TransportErrorKind {
    match code {
        "subscription_sharing_usage_limit_exceeded" | "rate_limit_exceeded" => TransportErrorKind::RateLimit,
        "subscription_sharing_invalid_user" | "chatpass_v2_scope_not_authorized" | "chatpass_v2_invalid_authorization_context" => {
            TransportErrorKind::Auth
        }
        _ => TransportErrorKind::HttpStatus,
    }
}

/// Rewrites a non-200 answer's message to carry only the documented `error.code`,
/// `error.param` and recovery hint (never raw server message/detail/body text).
fn describe_http_error(mut err: TransportError, secret: &str) -> TransportError {
    let Some(status_code) = err.status else {
        err.message = scrub_text(&err.message, secret);
        return err;
    };
    let status = format!("HTTP {status_code}");
    if let Some(v) = err.raw_body.as_deref().and_then(|b| serde_json::from_slice::<Value>(b).ok()) {
        if let Some(code) = clip_field(&v["error"]["code"]) {
            let param = clip_field(&v["error"]["param"]);
            let mut msg = format!("{status} {code}");
            if let Some(p) = param {
                msg.push_str(&format!(" (param {p})"));
            }
            if let Some(h) = recovery_hint(&code) {
                msg.push_str(&format!(": {h}"));
            }
            err.message = scrub_text(&msg, secret);
            return err;
        }
        if v["detail"].as_str().is_some() {
            // Direct-route admission failures may answer `{"detail": "..."}`; keep only the status and fixed hint.
            let hint = match status_code {
                401 => "the sign-in or its ChatGPT plan permission was not accepted; sign in again",
                403 => "a policy or permission check (for example the serving region) refused the request",
                503 => "ChatGPT plan routing is unavailable right now; try again later",
                _ => "request refused",
            };
            err.message = format!("{status}: {hint}");
            return err;
        }
    }
    err.message = if err.raw_body.is_some() { status } else { scrub_text(&err.message, secret) };
    err
}

fn stream_error(clean: Vec<u8>, kind: TransportErrorKind, message: String) -> TransportError {
    TransportError { kind, status: Some(200), message, raw_body: Some(clean) }
}

fn format_stream_code_error(prefix: &str, code: &str, param: Option<&str>, secret: &str) -> String {
    let mut msg = format!("{prefix}: {code}");
    if let Some(p) = param {
        msg.push_str(&format!(" (param {p})"));
    }
    if let Some(h) = recovery_hint(code) {
        msg.push_str(&format!(": {h}"));
    }
    scrub_text(&msg, secret)
}

/// Parse a streamed Responses answer (the raw `text/event-stream` bytes). Pure; also used
/// to rehydrate history on resume from the recorded stream.
pub fn parse_responses_stream(raw_body: Vec<u8>, secret: &str) -> Result<ModelResponse, TransportError> {
    let clean = scrub_bytes(&raw_body, secret.as_bytes());
    let malformed = |msg: String| malformed_response_err(clean.clone(), secret, msg);
    let text = std::str::from_utf8(&clean).map_err(|_| malformed("event stream is not UTF-8".into()))?;

    let mut done_items: Vec<Value> = Vec::new();
    let mut terminal: Option<Value> = None;
    for (name, data) in sse_events(text) {
        if data.trim().is_empty() || data.trim() == "[DONE]" {
            continue;
        }
        let ev: Value = serde_json::from_str(&data).map_err(|e| malformed(format!("invalid event JSON: {e}")))?;
        let ty = ev.get("type").and_then(Value::as_str).map(str::to_string).or(name).unwrap_or_default();
        match ty.as_str() {
            "response.output_item.done" => {
                if let Some(item) = ev.get("item") {
                    done_items.push(item.clone());
                }
            }
            "response.completed" => terminal = Some(ev.get("response").cloned().unwrap_or(Value::Null)),
            "response.incomplete" => {
                let reason = clip_field(&ev["response"]["incomplete_details"]["reason"]).unwrap_or_else(|| "unknown".into());
                let msg = scrub_text(&format!("response.incomplete: {reason}"), secret);
                return Err(stream_error(clean.clone(), kind_for_code(&reason), msg));
            }
            "response.failed" => {
                let code = clip_field(&ev["response"]["error"]["code"]).unwrap_or_else(|| "unknown_error".into());
                let param = clip_field(&ev["response"]["error"]["param"]);
                let msg = format_stream_code_error("response.failed", &code, param.as_deref(), secret);
                return Err(stream_error(clean.clone(), kind_for_code(&code), msg));
            }
            "error" => {
                let code = clip_field(&ev["code"]).or_else(|| clip_field(&ev["error"]["code"])).unwrap_or_else(|| "error".into());
                let param = clip_field(&ev["param"]).or_else(|| clip_field(&ev["error"]["param"]));
                let msg = format_stream_code_error("stream error", &code, param.as_deref(), secret);
                return Err(stream_error(clean.clone(), kind_for_code(&code), msg));
            }
            _ => {}
        }
    }

    let Some(response) = terminal else {
        return Err(malformed("stream ended without response.completed".into()));
    };
    if let Some(status) = response.get("status") {
        if status.as_str() != Some("completed") {
            let s = status.as_str().map(clip_code).unwrap_or_else(|| "invalid".into());
            return Err(malformed(format!("response.completed has non-completed status: {s}")));
        }
    }
    let model = response.get("model").and_then(Value::as_str)
        .ok_or_else(|| malformed("response missing `model` string".into()))?.to_string();
    let id = response.get("id").and_then(Value::as_str).map(str::to_string);
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);
    let output = match response.get("output").and_then(Value::as_array) {
        Some(items) if !items.is_empty() => items.clone(),
        _ => done_items,
    };

    let mut content = Vec::new();
    let mut refused = false;
    for (idx, item) in output.iter().enumerate() {
        decode_item(idx, item, &mut content, &mut refused).map_err(&malformed)?;
    }
    let has_tools = content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. }));
    let stop_reason = if refused {
        StopReason::Refusal
    } else if has_tools {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };
    Ok(ModelResponse { raw_body: clean, id, model, stop_reason, content, usage })
}

fn decode_item(idx: usize, item: &Value, out: &mut Vec<ContentBlock>, refused: &mut bool) -> Result<(), String> {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "message" => {
            for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            out.push(ContentBlock::Text { text: t.to_string() });
                        }
                    }
                    Some("refusal") => {
                        *refused = true;
                        if let Some(t) = part.get("refusal").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            out.push(ContentBlock::Text { text: t.to_string() });
                        }
                    }
                    _ => {}
                }
            }
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty())
                .ok_or_else(|| format!("output[{idx}] function_call missing `name`"))?;
            let id = item.get("call_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                .ok_or_else(|| format!("output[{idx}] function_call missing `call_id`"))?;
            let input = match item.get("arguments") {
                Some(Value::String(raw)) if raw.trim().is_empty() => json!({}),
                Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
                    .map_err(|e| format!("output[{idx}] function_call arguments invalid JSON: {e}"))?,
                Some(v @ Value::Object(_)) => v.clone(),
                _ => return Err(format!("output[{idx}] function_call missing `arguments`")),
            };
            if !input.is_object() {
                return Err(format!("output[{idx}] function_call arguments must be a JSON object"));
            }
            out.push(ContentBlock::ToolUse { id: id.to_string(), name: name.to_string(), input });
        }
        "reasoning" => {
            let summary: Vec<&str> = item.get("summary").and_then(Value::as_array).into_iter().flatten()
                .filter_map(|s| s.get("text").and_then(Value::as_str)).collect();
            out.push(ContentBlock::Thinking { thinking: summary.join("\n"), signature: None });
        }
        other => out.push(ContentBlock::Other { block_type: format!("responses:{}", clip_code(other)), raw: Value::Null }),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_bom_crlf_names_multiline_and_drops_unterminated() {
        let evs = sse_events("\u{FEFF}event: a\r\ndata: {\"x\":\r\ndata: 1}\r\n\r\n: comment\n\ndata: [DONE]\n\ndata: cut");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].0.as_deref(), Some("a"));
        assert_eq!(evs[0].1, "{\"x\":\n1}");
        assert_eq!(evs[1].1, "[DONE]");
    }

    #[test]
    fn only_official_host_or_loopback_is_allowed() {
        let ok = |u: &str| chatgpt_base_allowed(&validate_base_url(u).unwrap());
        for u in [
            "https://api.openai.com", "https://api.openai.com/v1", "https://api.openai.com/v1/responses",
            "http://127.0.0.1:9", "http://127.0.0.1:9/v1", "http://127.0.0.1:9/v1/responses",
        ] {
            assert!(ok(u), "{u}");
        }
        for u in [
            "https://proxy.example", "https://api.openai.com.evil.example", "https://api.openai.com/evil/prefix",
            "http://localhost:9", "http://[::1]:9", "http://127.0.0.1:9/evil",
        ] {
            assert!(!ok(u), "{u}");
        }
    }

    #[test]
    fn http_errors_carry_code_param_and_hint_but_no_secret_or_raw_text() {
        let mk = |kind, status, msg: &str, body: &[u8]| describe_http_error(
            TransportError { kind, status: Some(status), message: msg.into(), raw_body: Some(body.to_vec()) },
            "tok-123",
        );
        let e = mk(TransportErrorKind::RateLimit, 429, "x", br#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","param":"model","message":"tok-123 partial-leak"}}"#);
        assert!(e.message.contains("subscription_sharing_usage_limit_exceeded") && e.message.contains("(param model)") && e.message.contains("Usage"));
        assert!(!e.message.contains("tok-123") && !e.message.contains("partial-leak"));
        let e = mk(TransportErrorKind::HttpStatus, 400, "HTTP 400: raw-leak", br#"{"error":{"code":"invalid_request_error","param":"tools","message":"partial-tok-leak"}}"#);
        assert_eq!(e.message, "HTTP 400 invalid_request_error (param tools)");
        let e = mk(TransportErrorKind::Auth, 401, "x", br#"{"detail":"raw-detail-leak"}"#);
        assert!(e.message.contains("sign in again") && !e.message.contains("raw-detail-leak"));
        let e = mk(TransportErrorKind::HttpStatus, 502, "HTTP 502: raw-html-leak", b"<html>raw-html-leak</html>");
        assert_eq!(e.message, "HTTP 502");
    }

    #[test]
    fn serialize_rejects_empty_or_blank_user_turn_without_emitting_empty_input() {
        for txt in ["", "   \n\t "] {
            let req = ModelRequest { model: "gpt-6.2-sol".into(), system: Some("sys".into()), messages: vec![Message { role: Role::User, content: vec![ContentBlock::Text { text: txt.into() }] }], tools: Vec::new(), max_tokens: 1024, reasoning_effort: None };
            let err = serialize_responses_request(&req).expect_err("empty user turn must fail locally");
            assert!(err.kind == TransportErrorKind::Config && err.message.contains("no input messages"), "{err}");
        }
    }
}
