//! mcp — a Model Context Protocol server over stdio.
//!
//! Single responsibility: expose ghostie's memory operations to any MCP
//! client (Codex, Cursor, Claude, Windsurf, Zed) as tools, spoken as
//! newline-delimited JSON-RPC 2.0 over stdin/stdout. One JSON object per
//! line: read a line, handle it, write exactly one response line, flush.
//!
//! The protocol handling is a pure function, [`handle_message`], which takes
//! a request line and returns the response line (or `None` for a
//! notification, which by JSON-RPC gets no reply). [`serve`] is the thin
//! stdio loop around it, so the interesting logic is unit-tested without
//! spawning a process or touching real stdio.
//!
//! Tools exposed: `recall`, `remember`, `capture`, `list`. A tool that fails
//! (bad arguments, a store error) reports the failure *in band* as an MCP
//! tool result with `isError: true`, so the model sees it; only an unknown
//! JSON-RPC *method* is a protocol-level error object.

use crate::capture;
use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::recall::{RecallOpts, recall};
use crate::store::memory::MemoryType;
use crate::store::{ListFilter, NewMemory, Store};
use crate::util::{Clock, resolve_clock};

/// The MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";
/// The server name reported in `initialize` and the manifest.
pub const SERVER_NAME: &str = "ghostie";

/// The server version (the crate version, kept in lockstep).
pub fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------- tool schemas ----------

fn schema_string(desc: &str) -> Value {
    Value::Object(vec![
        ("type".to_string(), Value::string("string")),
        ("description".to_string(), Value::string(desc)),
    ])
}

fn schema_integer(desc: &str) -> Value {
    Value::Object(vec![
        ("type".to_string(), Value::string("integer")),
        ("description".to_string(), Value::string(desc)),
    ])
}

fn schema_string_array(desc: &str) -> Value {
    Value::Object(vec![
        ("type".to_string(), Value::string("array")),
        (
            "items".to_string(),
            Value::Object(vec![("type".to_string(), Value::string("string"))]),
        ),
        ("description".to_string(), Value::string(desc)),
    ])
}

/// Build a JSON Schema object node from named properties and the required set.
fn input_schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    Value::Object(vec![
        ("type".to_string(), Value::string("object")),
        (
            "properties".to_string(),
            Value::Object(props.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
        ),
        (
            "required".to_string(),
            Value::Array(required.iter().map(|r| Value::string(*r)).collect()),
        ),
    ])
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    Value::Object(vec![
        ("name".to_string(), Value::string(name)),
        ("description".to_string(), Value::string(description)),
        ("inputSchema".to_string(), schema),
    ])
}

/// The tool catalog, in fixed order (byte-stable). Shared by `tools/list`
/// and the CLI manifest so the two never drift.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "recall",
            "Retrieve the most relevant memories for a task or question. Returns \
             ranked cards, each with the memory's title, why it surfaced, and its \
             provenance and store path.",
            input_schema(
                vec![
                    (
                        "query",
                        schema_string("The task or question to recall for."),
                    ),
                    (
                        "budget",
                        schema_integer("Approximate token budget for the whole result."),
                    ),
                    ("k", schema_integer("Maximum number of memories to return.")),
                    (
                        "scope",
                        schema_string("Limit to a scope, e.g. 'global' or 'project:<name>'."),
                    ),
                ],
                &["query"],
            ),
        ),
        tool(
            "remember",
            "Create a new memory. Returns the new memory's id.",
            input_schema(
                vec![
                    ("type", schema_string("fact | decision | rule.")),
                    ("title", schema_string("One-line title (required).")),
                    ("body", schema_string("Optional Markdown body.")),
                    ("tags", schema_string_array("Optional tags.")),
                    (
                        "harness",
                        schema_string("Provenance: where it was made (e.g. codex)."),
                    ),
                    (
                        "core",
                        schema_string("Provenance: which model produced it."),
                    ),
                    (
                        "rationale",
                        schema_string("One-line reason this memory matters."),
                    ),
                    (
                        "scope",
                        schema_string("Retrieval scope: 'global' or 'project:<name>'."),
                    ),
                ],
                &["type", "title"],
            ),
        ),
        tool(
            "capture",
            "Distill an agent session transcript into memories (a session-summary \
             plus any 'MEMORY <type>:' markers). Returns the created ids.",
            input_schema(
                vec![
                    (
                        "path",
                        schema_string("Path to the transcript file (required)."),
                    ),
                    (
                        "format",
                        schema_string("auto | claude-code | codex | generic."),
                    ),
                    ("harness", schema_string("Override the recorded harness.")),
                ],
                &["path"],
            ),
        ),
        tool(
            "list",
            "List every memory in the store (id, type, title), in deterministic order.",
            input_schema(vec![], &[]),
        ),
    ]
}

/// The one-shot manifest the CLI prints for bare `ghostie mcp`: server
/// identity plus the tool catalog. Satisfies the robot-mode contract without
/// starting the (blocking) stdio loop.
pub fn manifest_data() -> Value {
    Value::Object(vec![
        ("name".to_string(), Value::string(SERVER_NAME)),
        ("version".to_string(), Value::string(server_version())),
        (
            "protocolVersion".to_string(),
            Value::string(PROTOCOL_VERSION),
        ),
        ("tools".to_string(), Value::Array(tool_definitions())),
    ])
}

// ---------- JSON-RPC handling ----------

/// A JSON-RPC protocol-level error (not a tool failure).
struct RpcError {
    code: i64,
    message: String,
}

/// Handle one JSON-RPC line. Returns the response line to write, or `None`
/// when the message is a notification (no `id`), which gets no reply. Pure:
/// no stdio, no globals — the whole protocol is exercised from unit tests.
pub fn handle_message(store: &Store, line: &str, clock: &dyn Clock) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let msg = match json::parse(trimmed) {
        Ok(v) => v,
        Err(e) => {
            // Malformed JSON: a parse error with a null id per JSON-RPC 2.0.
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            ));
        }
    };
    // A request carries an id; a notification does not (and gets no reply).
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match dispatch(store, method, msg.get("params"), clock) {
        Ok(result) => Some(success_response(id, result)),
        Err(rpc) => Some(error_response(id, rpc.code, &rpc.message)),
    }
}

fn dispatch(
    store: &Store,
    method: &str,
    params: Option<&Value>,
    clock: &dyn Clock,
) -> std::result::Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize_result()),
        "tools/list" => Ok(Value::Object(vec![(
            "tools".to_string(),
            Value::Array(tool_definitions()),
        )])),
        "tools/call" => Ok(tools_call(store, params, clock)),
        _ => Err(RpcError {
            code: -32601,
            message: "method not found".to_string(),
        }),
    }
}

fn initialize_result() -> Value {
    Value::Object(vec![
        (
            "protocolVersion".to_string(),
            Value::string(PROTOCOL_VERSION),
        ),
        (
            "capabilities".to_string(),
            Value::Object(vec![("tools".to_string(), Value::Object(vec![]))]),
        ),
        (
            "serverInfo".to_string(),
            Value::Object(vec![
                ("name".to_string(), Value::string(SERVER_NAME)),
                ("version".to_string(), Value::string(server_version())),
            ]),
        ),
    ])
}

/// Dispatch a `tools/call`. A tool failure is reported in band as a tool
/// result with `isError: true` (so the model sees it), never as a JSON-RPC
/// error — those are reserved for protocol faults.
fn tools_call(store: &Store, params: Option<&Value>, clock: &dyn Clock) -> Value {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let empty = Value::Object(vec![]);
    let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
    let outcome = match name {
        "recall" => tool_recall(store, args),
        "remember" => tool_remember(store, args, clock),
        "capture" => tool_capture(store, args, clock),
        "list" => tool_list(store),
        other => Err(format!(
            "unknown tool '{other}' (recall | remember | capture | list)"
        )),
    };
    match outcome {
        Ok(text) => tool_result(&text, false),
        Err(text) => tool_result(&text, true),
    }
}

fn tool_result(text: &str, is_error: bool) -> Value {
    Value::Object(vec![
        (
            "content".to_string(),
            Value::Array(vec![Value::Object(vec![
                ("type".to_string(), Value::string("text")),
                ("text".to_string(), Value::string(text)),
            ])]),
        ),
        ("isError".to_string(), Value::Bool(is_error)),
    ])
}

fn success_response(id: Value, result: Value) -> String {
    Value::Object(vec![
        ("jsonrpc".to_string(), Value::string("2.0")),
        ("id".to_string(), id),
        ("result".to_string(), result),
    ])
    .emit()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    Value::Object(vec![
        ("jsonrpc".to_string(), Value::string("2.0")),
        ("id".to_string(), id),
        (
            "error".to_string(),
            Value::Object(vec![
                ("code".to_string(), Value::int(code)),
                ("message".to_string(), Value::string(message)),
            ]),
        ),
    ])
    .emit()
}

// ---------- argument helpers ----------

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_opt_string(args: &Value, key: &str) -> Option<String> {
    arg_str(args, key).map(str::to_string)
}

fn arg_string_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ---------- tools ----------

fn tool_recall(store: &Store, args: &Value) -> std::result::Result<String, String> {
    let query =
        arg_str(args, "query").ok_or_else(|| "recall requires a 'query' string".to_string())?;
    let mut opts = RecallOpts::default();
    if let Some(k) = args.get("k").and_then(Value::as_i64)
        && k >= 1
    {
        opts.k = k as usize;
    }
    if let Some(b) = args.get("budget").and_then(Value::as_i64)
        && b >= 0
    {
        opts.budget_tokens = Some(b as usize);
    }
    opts.scope = arg_opt_string(args, "scope");
    let result = recall(store, query, &opts).map_err(|e| e.to_string())?;
    if result.hits.is_empty() {
        return Ok("no memories matched".to_string());
    }
    let mut out = String::new();
    for (i, hit) in result.hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}  [{}]{}\n",
            i + 1,
            hit.title,
            hit.mtype.as_str(),
            hit.provenance_tag(),
        ));
        if let Some(rationale) = &hit.rationale {
            out.push_str(&format!("   why it matters: {rationale}\n"));
        }
        out.push_str(&format!("   {}\n", hit.why_line()));
        out.push_str(&format!("   {}\n", hit.path));
    }
    Ok(out.trim_end().to_string())
}

fn tool_remember(
    store: &Store,
    args: &Value,
    clock: &dyn Clock,
) -> std::result::Result<String, String> {
    let type_str = arg_str(args, "type")
        .ok_or_else(|| "remember requires a 'type' (fact | decision | rule)".to_string())?;
    if type_str == "session-summary" {
        return Err("session summaries come from capture, not remember".to_string());
    }
    let mtype = MemoryType::parse(type_str)
        .filter(|t| *t != MemoryType::SessionSummary)
        .ok_or_else(|| format!("unknown type '{type_str}' (fact | decision | rule)"))?;
    let title = arg_str(args, "title").ok_or_else(|| "remember requires a 'title'".to_string())?;
    if title.trim().is_empty() {
        return Err("title must not be empty".to_string());
    }
    let memory = store
        .create(
            &NewMemory {
                mtype: Some(mtype),
                title: title.to_string(),
                tags: arg_string_array(args, "tags"),
                harness: arg_opt_string(args, "harness"),
                core: arg_opt_string(args, "core"),
                rationale: arg_opt_string(args, "rationale"),
                scope: arg_opt_string(args, "scope"),
                body: arg_opt_string(args, "body").unwrap_or_default(),
                ..NewMemory::default()
            },
            clock,
        )
        .map_err(|e| e.to_string())?;
    Ok(format!("created {} (memories/{}.md)", memory.id, memory.id))
}

fn tool_capture(
    store: &Store,
    args: &Value,
    clock: &dyn Clock,
) -> std::result::Result<String, String> {
    let path = arg_str(args, "path").ok_or_else(|| "capture requires a 'path'".to_string())?;
    let format = match arg_str(args, "format") {
        Some(f) => capture::Format::parse(f).ok_or_else(|| {
            format!("unknown format '{f}' (auto | claude-code | codex | generic)")
        })?,
        None => None,
    };
    let harness = arg_opt_string(args, "harness");
    let created = capture::capture_file(store, path, format, harness.as_deref(), None, None, clock)
        .map_err(|e| e.to_string())?;
    if created.is_empty() {
        return Ok("captured 0 memories".to_string());
    }
    let mut out = format!("captured {} memory(ies):\n", created.len());
    for m in &created {
        out.push_str(&format!(
            "  {}  [{}]  {}\n",
            m.id,
            m.mtype.as_str(),
            m.title
        ));
    }
    Ok(out.trim_end().to_string())
}

fn tool_list(store: &Store) -> std::result::Result<String, String> {
    let (memories, _warnings) = store
        .list(&ListFilter::default())
        .map_err(|e| e.to_string())?;
    if memories.is_empty() {
        return Ok("no memories in the store".to_string());
    }
    let mut out = String::new();
    for m in &memories {
        out.push_str(&format!("{}  [{}]  {}\n", m.id, m.mtype.as_str(), m.title));
    }
    Ok(out.trim_end().to_string())
}

// ---------- stdio loop ----------

fn stdout_io(e: std::io::Error) -> Error {
    Error::Io {
        context: "writing MCP response to stdout".to_string(),
        path: "<stdout>".to_string(),
        source: e,
    }
}

/// Run the MCP server: read newline-delimited JSON-RPC from stdin, write one
/// response line per request to stdout, flushing each so the client sees
/// replies promptly. Returns on stdin EOF.
pub fn serve(store: &Store) -> Result<()> {
    use std::io::{BufRead, Write};
    let clock = resolve_clock()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Error::Io {
            context: "reading MCP request from stdin".to_string(),
            path: "<stdin>".to_string(),
            source: e,
        })?;
        if let Some(response) = handle_message(store, &line, clock.as_ref()) {
            out.write_all(response.as_bytes()).map_err(stdout_io)?;
            out.write_all(b"\n").map_err(stdout_io)?;
            out.flush().map_err(stdout_io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

    fn store_at(tmp: &TempDir) -> Store {
        Store::open(tmp.path())
    }

    fn call(store: &Store, line: &str) -> Value {
        let clock = FixedClock(T0);
        let out = handle_message(store, line, &clock).expect("request gets a response");
        json::parse(&out).expect("response is valid JSON")
    }

    #[test]
    fn initialize_reports_server_info() {
        let tmp = TempDir::new("mcp-init");
        let store = store_at(&tmp);
        let doc = call(&store, r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(doc.get("id").and_then(Value::as_i64), Some(1));
        let result = doc.get("result").unwrap();
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some(PROTOCOL_VERSION)
        );
        let info = result.get("serverInfo").unwrap();
        assert_eq!(info.get("name").and_then(Value::as_str), Some("ghostie"));
        assert_eq!(
            info.get("version").and_then(Value::as_str),
            Some(server_version())
        );
        // capabilities.tools is present (an object).
        assert!(
            result
                .get("capabilities")
                .and_then(|c| c.get("tools"))
                .is_some()
        );
    }

    #[test]
    fn notifications_get_no_response() {
        let tmp = TempDir::new("mcp-notif");
        let store = store_at(&tmp);
        let clock = FixedClock(T0);
        let out = handle_message(
            &store,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &clock,
        );
        assert!(out.is_none(), "a notification gets no reply");
    }

    #[test]
    fn tools_list_advertises_all_four_tools() {
        let tmp = TempDir::new("mcp-tools");
        let store = store_at(&tmp);
        let doc = call(&store, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = doc
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
            .unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["recall", "remember", "capture", "list"]);
        // Every tool carries a JSON Schema object inputSchema.
        for t in tools {
            let schema = t.get("inputSchema").unwrap();
            assert_eq!(schema.get("type").and_then(Value::as_str), Some("object"));
            assert!(schema.get("properties").is_some());
        }
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let tmp = TempDir::new("mcp-badmethod");
        let store = store_at(&tmp);
        let doc = call(&store, r#"{"jsonrpc":"2.0","id":9,"method":"no/such"}"#);
        assert!(doc.get("result").is_none());
        let err = doc.get("error").unwrap();
        assert_eq!(err.get("code").and_then(Value::as_i64), Some(-32601));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let tmp = TempDir::new("mcp-parse");
        let store = store_at(&tmp);
        let clock = FixedClock(T0);
        let out = handle_message(&store, "{not json", &clock).unwrap();
        let doc = json::parse(&out).unwrap();
        assert_eq!(
            doc.get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_i64),
            Some(-32700)
        );
        assert!(matches!(doc.get("id"), Some(Value::Null)));
    }

    #[test]
    fn remember_then_list_then_recall_round_trip() {
        let tmp = TempDir::new("mcp-roundtrip");
        let store = store_at(&tmp);
        // remember
        let doc = call(
            &store,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remember","arguments":{"type":"rule","title":"Always run verify before commit","tags":["ci"],"rationale":"the gate is verify.sh"}}}"#,
        );
        let result = doc.get("result").unwrap();
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            text.contains("rule-always-run-verify-before-commit-1"),
            "{text}"
        );
        // list shows it
        let doc = call(
            &store,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list","arguments":{}}}"#,
        );
        let text = doc
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            text.contains("rule-always-run-verify-before-commit-1"),
            "{text}"
        );
        // recall finds it (and is not an error)
        let doc = call(
            &store,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"recall","arguments":{"query":"what do I run before commit"}}}"#,
        );
        let result = doc.get("result").unwrap();
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.contains("Always run verify before commit"), "{text}");
    }

    #[test]
    fn tool_failure_is_in_band_not_a_protocol_error() {
        let tmp = TempDir::new("mcp-toolfail");
        let store = store_at(&tmp);
        // Missing required 'title' -> tool result with isError:true, still a
        // JSON-RPC success envelope (result, not error).
        let doc = call(
            &store,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"remember","arguments":{"type":"fact"}}}"#,
        );
        assert!(
            doc.get("error").is_none(),
            "a tool failure is not a protocol error"
        );
        let result = doc.get("result").unwrap();
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn unknown_tool_reports_in_band() {
        let tmp = TempDir::new("mcp-unknowntool");
        let store = store_at(&tmp);
        let doc = call(
            &store,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"frobnicate","arguments":{}}}"#,
        );
        let result = doc.get("result").unwrap();
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn capture_tool_ingests_a_transcript() {
        let tmp = TempDir::new("mcp-capture");
        let store = store_at(&tmp);
        let transcript = tmp.path().join("session.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"MEMORY fact: the config lives in etc"}}"#,
        )
        .unwrap();
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{{"name":"capture","arguments":{{"path":"{}"}}}}}}"#,
            transcript.display()
        );
        let doc = call(&store, &line);
        let result = doc.get("result").unwrap();
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(false));
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.contains("captured"), "{text}");
    }

    #[test]
    fn manifest_lists_tools_and_identity() {
        let m = manifest_data();
        assert_eq!(m.get("name").and_then(Value::as_str), Some("ghostie"));
        assert_eq!(
            m.get("protocolVersion").and_then(Value::as_str),
            Some(PROTOCOL_VERSION)
        );
        let tools = m.get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(tools.len(), 4);
    }
}
