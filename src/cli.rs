//! cli — the command-line surface: `remember`, `recall`, `list` (and later
//! `capture`, `sync`), each with a `--json` robot mode.
//!
//! Single responsibility: parse args, call the library, render (human and
//! JSON). All rendering lives here in the library so unit tests cover it
//! without spawning processes; `main.rs` stays a thin shim.
//!
//! # Exit-code contract (fixed; agents branch on these — never repurpose)
//!
//! - `0` success
//! - `1` operational failure (I/O, parse, store error)
//! - `2` usage error (bad args; usage goes to stderr)
//! - `3` sync conflicts (reserved for the sync phase)
//!
//! # Robot mode contract
//!
//! With `--json`: exactly one JSON document on stdout, schema
//! `{ "ok": bool, "command": str, "data": {...} | "error": { "kind",
//! "message" }, "warnings": [{origin, message}] }`, byte-stable given the
//! same store and args. No ANSI, no progress noise; diagnostics to stderr.
//! Errors still exit non-zero AND emit the error document.

use crate::error::{Error, Warning};
use crate::json::Value;
use crate::recall::{RecallOpts, recall};
use crate::store::memory::MemoryType;
use crate::store::{ListFilter, NewMemory, Store};
use crate::util::{format_rfc3339_utc, resolve_clock};
use crate::{capture, codex, hook, sync};
use std::path::PathBuf;

/// Subcommands implemented so far, in help order. The robot audit and the
/// e2e suite iterate THIS list, so a new verb is auto-covered.
pub const SUBCOMMANDS: [&str; 9] = [
    "setup",
    "remember",
    "recall",
    "list",
    "capture",
    "sync",
    "hook",
    "mcp",
    "provenance",
];

/// Everything a command run produces; `run` prints it, tests assert on it.
#[derive(Debug, Default)]
pub struct CliOutput {
    /// Process exit code per the contract.
    pub exit_code: i32,
    /// Bytes for stdout (exactly one JSON document in robot mode).
    pub stdout: String,
    /// Bytes for stderr (diagnostics, usage, warnings in human mode).
    pub stderr: String,
}

/// Entry point called by the binary shim. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let out = execute(args, None);
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    out.exit_code
}

/// Testable core: run a CLI invocation, capturing output. `stdin_body`
/// substitutes for reading real stdin (`--body -`); pass `None` in
/// production (the real stdin is read only when the flag asks for it).
pub fn execute(args: &[String], stdin_body: Option<&str>) -> CliOutput {
    let parsed = match Globals::parse(args) {
        Ok(p) => p,
        Err(e) => return render_failure("(args)", false, &e, Vec::new()),
    };
    let json_mode = parsed.json;
    let Some(command) = parsed.rest.first().map(String::as_str) else {
        // No subcommand: usage to stderr, exit 2 (or the error envelope).
        let e = Error::Usage {
            message: "no command given; try `ghostie help`".to_string(),
        };
        return render_failure("(none)", json_mode, &e, Vec::new());
    };
    let rest = &parsed.rest[1..];
    match command {
        "help" | "--help" | "-h" => help(rest, json_mode),
        "version" | "--version" | "-V" => version(json_mode),
        "_subcommands" => subcommands_verb(json_mode),
        "remember" => wrap(command, &parsed, |store| {
            cmd_remember(store, rest, stdin_body, json_mode)
        }),
        "recall" => wrap(command, &parsed, |store| cmd_recall(store, rest, json_mode)),
        "list" => wrap(command, &parsed, |store| cmd_list(store, rest, json_mode)),
        "_recanon" => wrap(command, &parsed, cmd_recanon),
        "capture" => wrap(command, &parsed, |store| {
            cmd_capture(store, rest, json_mode)
        }),
        "sync" => wrap(command, &parsed, |store| cmd_sync(store, rest, json_mode)),
        "hook" => wrap(command, &parsed, |store| cmd_hook(store, rest, json_mode)),
        "mcp" => wrap(command, &parsed, |store| cmd_mcp(store, rest, json_mode)),
        "provenance" => wrap(command, &parsed, |store| {
            cmd_provenance(store, rest, json_mode)
        }),
        "setup" => wrap(command, &parsed, |store| cmd_setup(store, rest, json_mode)),
        other => {
            let e = Error::Usage {
                message: format!("unknown command '{other}'; try `ghostie help`"),
            };
            render_failure(other, json_mode, &e, Vec::new())
        }
    }
}

/// One command's successful payload.
struct CmdOk {
    /// Robot-mode data object.
    data: Value,
    /// Human-mode stdout (ignored in robot mode).
    human: String,
    /// Structured warnings (both modes).
    warnings: Vec<Warning>,
}

fn wrap(
    command: &str,
    globals: &Globals,
    f: impl FnOnce(&Store) -> Result<CmdOk, Error>,
) -> CliOutput {
    let store = Store::open(globals.store_root.clone());
    match f(&store) {
        Ok(ok) => render_success(command, globals, ok),
        Err(e) => render_failure(command, globals.json, &e, Vec::new()),
    }
}

fn render_success(command: &str, globals: &Globals, ok: CmdOk) -> CliOutput {
    let mut out = CliOutput::default();
    if globals.json {
        let envelope = Value::Object(vec![
            ("ok".to_string(), Value::Bool(true)),
            ("command".to_string(), Value::string(command)),
            ("data".to_string(), ok.data),
            ("warnings".to_string(), warnings_json(&ok.warnings)),
        ]);
        out.stdout = envelope.emit();
        out.stdout.push('\n');
    } else {
        out.stdout = ok.human;
        if !globals.quiet {
            for w in &ok.warnings {
                out.stderr.push_str(&format!("warning: {w}\n"));
            }
        }
    }
    out.exit_code = 0;
    out
}

fn render_failure(command: &str, json_mode: bool, e: &Error, warnings: Vec<Warning>) -> CliOutput {
    let mut out = CliOutput {
        exit_code: match e {
            Error::Usage { .. } => 2,
            Error::Conflict { .. } => 3,
            _ => 1,
        },
        ..CliOutput::default()
    };
    if json_mode {
        let kind = match e {
            Error::Usage { .. } => "usage",
            Error::Io { .. } => "io",
            Error::Parse { .. } => "parse",
            Error::Invalid { .. } => "invalid",
            Error::InvalidTimestamp { .. } => "timestamp",
            Error::Conflict { .. } => "conflict",
        };
        let envelope = Value::Object(vec![
            ("ok".to_string(), Value::Bool(false)),
            ("command".to_string(), Value::string(command)),
            (
                "error".to_string(),
                Value::Object(vec![
                    ("kind".to_string(), Value::string(kind)),
                    ("message".to_string(), Value::string(e.to_string())),
                ]),
            ),
            ("warnings".to_string(), warnings_json(&warnings)),
        ]);
        out.stdout = envelope.emit();
        out.stdout.push('\n');
    } else {
        out.stderr = format!("ghostie: {e}\n");
        if out.exit_code == 2 {
            out.stderr.push_str("run `ghostie help` for usage\n");
        }
    }
    out
}

fn warnings_json(warnings: &[Warning]) -> Value {
    Value::Array(
        warnings
            .iter()
            .map(|w| {
                Value::Object(vec![
                    ("origin".to_string(), Value::string(w.origin.clone())),
                    ("message".to_string(), Value::string(w.message.clone())),
                ])
            })
            .collect(),
    )
}

/// Global flags + remaining args.
struct Globals {
    json: bool,
    quiet: bool,
    store_root: PathBuf,
    rest: Vec<String>,
}

impl Globals {
    /// Accept global flags anywhere; everything else stays in order.
    /// Precedence for the store root: `--store` flag > `$GHOSTIE_HOME` >
    /// `~/.ghostie`.
    fn parse(args: &[String]) -> Result<Globals, Error> {
        let mut json = false;
        let mut quiet = false;
        let mut store_flag: Option<PathBuf> = None;
        let mut rest = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            match a.as_str() {
                "--json" => json = true,
                "--quiet" => quiet = true,
                "--store" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| Error::Usage {
                        message: "--store requires a path".to_string(),
                    })?;
                    store_flag = Some(PathBuf::from(v));
                }
                _ if a.starts_with("--store=") => {
                    store_flag = Some(PathBuf::from(&a["--store=".len()..]));
                }
                _ => rest.push(a.clone()),
            }
            i += 1;
        }
        let store_root = match store_flag {
            Some(p) => p,
            None => match std::env::var_os("GHOSTIE_HOME") {
                Some(home) => PathBuf::from(home),
                None => {
                    let home = std::env::var_os("HOME").ok_or_else(|| Error::Usage {
                        message: "cannot locate the store: set --store, $GHOSTIE_HOME or $HOME"
                            .to_string(),
                    })?;
                    PathBuf::from(home).join(".ghostie")
                }
            },
        };
        Ok(Globals {
            json,
            quiet,
            store_root,
            rest,
        })
    }
}

// ---------- help / version ----------

const HELP: &str = "\
ghostie — memory you own (plain files, deterministic, provider-agnostic)

USAGE
  ghostie <command> [args] [--json] [--store <path>] [--quiet]

COMMANDS
  setup      one command to make it just work (see below)
  remember   create a memory from the command line
  recall     query the store; ranked hits, each with a why
  list       enumerate memories deterministically
  capture    distill a session log into memories
  sync       sync the store via your own git remote (--init <remote> first)
  hook       auto-recall / auto-capture: `hook install`, `status`, `uninstall`
  mcp        serve ghostie as an MCP server (`mcp serve`) for any MCP client
  provenance show a memory's lineage; `provenance verify` replays the chain
  help       this text; `ghostie help <command>` for details
  version    print the version

ONE BUTTON (make it just work)
  ghostie setup <your-git-remote>   # wire sync + install auto-recall/capture
                                    # + first push, in one command
  ghostie setup                     # local only (hooks, no cross-device sync)

GLOBAL FLAGS
  --json           robot mode: exactly one JSON document on stdout
  --store <path>   store root (precedence: flag > $GHOSTIE_HOME > ~/.ghostie)
  --quiet          suppress human-mode warnings

EXIT CODES
  0 success · 1 operational failure · 2 usage error · 3 sync conflicts
";

const HELP_REMEMBER: &str = "\
ghostie remember --type fact|decision|rule \"title\" [options]

  --type <t>       fact | decision | rule (session-summary comes from
                   `ghostie capture`, not remember — provenance matters)
  --body <text>    memory body; `--body -` reads the body from stdin
  --tags a,b       comma-separated tags
  --link <id>      link to a memory id (repeatable)
  --supersedes <id> (decisions) the decision this one replaces
  --why <text>     why this is necessary; the card's why-line (alias --rationale)
  --harness <h>    provenance: where it was made (claude-code | hermes | codex)
  --core <m>       provenance: which model produced it (opus-4.8 | hermes-4-405b)
  --scope <s>      global (default) | project:<name>, to keep recall focused
  --no-redact      store content verbatim; skip the default secret scrubbing

EXAMPLES
  ghostie remember --type rule \"Always run verify.sh before commit\" --tags ci
  ghostie remember --type decision \"Chose Postgres over Mongo\" \\
    --why \"we'll hit this again porting the API\" --harness hermes --core hermes-4-405b
  echo \"body text\" | ghostie remember --type fact \"Configs live in etc\" --body - --json

EXIT CODES: 0 created · 1 store failure · 2 bad arguments
";

const HELP_RECALL: &str = "\
ghostie recall \"<task or question>\" [-k N] [--type t] [--tag t]
                                    [--scope s] [--budget N] [--json]

  The query is ONE positional argument — quote multi-word queries.
  -k <N>        max hits (default 10)
  --type <t>    only memories of this type
  --tag <t>     only memories carrying this tag
  --scope <s>   only this scope (global | project:<name>)
  --budget <N>  cap the result at ~N tokens; packs the best cards and stops,
                so a context-injection hook never floods (top card always kept)
  --diverse     demote near-duplicate memories (MMR); pairs well with --budget
  --no-rerank   disable the semantic (hashed-embedding) rerank; BM25 only

  Hits reached through the link graph (Personalized PageRank) rather than by
  word match are labelled \"reached via link from <id>\".

EXAMPLES
  ghostie recall \"which branch do we sync to\"
  ghostie recall \"tokenizer bug\" -k 3 --json
  ghostie recall \"auth approach\" --budget 800   # for a session-start hook

EXIT CODES: 0 (zero hits is an answer, not an error) · 1 failure · 2 bad arguments
";

const HELP_LIST: &str = "\
ghostie list [--type t] [--tag t] [--rebuild-index] [--json]

  Deterministic order: id, lexicographic ascending.
  --type <t>        only this type
  --tag <t>         only this tag
  --rebuild-index   force a full rebuild of the derivable index first

EXAMPLES
  ghostie list --type rule
  ghostie list --json

EXIT CODES: 0 success · 1 failure · 2 bad arguments
";

const HELP_SETUP: &str = "\
ghostie setup [<git-remote>] [--budget N]

  The one button. With a remote: wires the store to your own git remote,
  installs the recall-on-prompt and capture-on-end hooks (with sync), and does
  the first push. Without a remote: installs the hooks local-only.
  --budget <N>   token budget for the recall-on-prompt injection (default 600)

EXAMPLES
  ghostie setup git@github.com:me/my-memory.git
  ghostie setup                       # local, no cross-device sync

EXIT CODES: 0 success · 1 failure · 2 bad arguments · 3 sync conflict
";

const HELP_CAPTURE: &str = "\
ghostie capture <transcript-path> [--format f] [--harness h] [--core c] [--scope s]
ghostie capture --latest codex [--scope s]

  Distill an agent session log into memories: a session-summary carrying
  provenance, plus one memory for each `MEMORY <type>: text` marker in the
  transcript. Re-capturing the same session is idempotent.

  --latest <h>    capture the newest rollout for harness <h> (codex) instead of
                  a path: finds the most recent ~/.codex rollout itself
  --format <f>    auto (default) | claude-code | codex | generic
                  auto sniffs the file; generic reads ANY text/markdown, so a
                  harness without a bespoke parser still works via markers
  --harness <h>   override the recorded harness (claude-code | codex | hermes)
  --core <c>      override the recorded model
  --scope <s>     stamp a retrieval scope (global | project:<name>)
  --no-redact     ingest verbatim; skip the default secret scrubbing

  By default capture scrubs detected secrets (API keys, tokens, private keys)
  from ingested content before it is written, so a transcript that echoed a key
  never lands in a memory file or syncs to your remote.

EXAMPLES
  ghostie capture ~/.codex/archived_sessions/rollout-*.jsonl
  ghostie capture --latest codex
  ghostie capture ~/.hermes/notes.md --harness hermes --format generic

EXIT CODES: 0 success · 1 failure · 2 bad arguments
";

const HELP_SYNC: &str = "\
ghostie sync [--init <git-remote>]

  --init <remote>   one-time: make the store a git repo pointed at your own
                    remote (the derived index is never synced)
  (no args)         commit local changes, rebase in remote changes, push

  Conflicts are reported, never auto-resolved (exit code 3): the working tree
  is left untouched for you to resolve, then re-run.

EXIT CODES: 0 success · 1 failure · 2 bad arguments · 3 sync conflict
";

const HELP_HOOK: &str = "\
ghostie hook <subcommand>

  install [--budget N] [--sync]   wire Claude Code to recall relevant memories
                                  on each prompt and capture (and optionally
                                  push) on session end; backs up settings first
  install --harness codex [--sync]
                                  wire Codex: set its `notify` program in
                                  ~/.codex/config.toml so each completed turn
                                  captures the just-finished rollout; backs up
                                  config first, and refuses to clobber a notify
                                  you already have (prints the line to paste)
  status [--harness codex]        show whether the hooks are installed
  uninstall [--harness codex]     remove ghostie's hooks, leave the rest
  run recall [--budget N]         runner: reads the hook payload on stdin,
                                  emits injectable context (invoked by install)
  run capture [--sync]            runner: captures the transcript named in the
                                  payload, optionally syncs
  run capture --codex-notify [--sync]
                                  runner Codex calls: reads the notify event
                                  (last argv arg) and captures the newest
                                  ~/.codex rollout; idempotent per session

EXIT CODES: 0 success · 1 failure · 2 bad arguments
";

const HELP_MCP: &str = "\
ghostie mcp [serve]

  Serve ghostie as a Model Context Protocol (MCP) server so any MCP client
  (Codex, Cursor, Claude, Windsurf, Zed) can use your store as its memory.

  serve       run the server: newline-delimited JSON-RPC 2.0 over stdin/stdout.
              Tools exposed: recall, remember, capture, list.
  (no args)   print a one-shot manifest (server name/version + the tool list)
              and exit; `mcp --json` emits it as a single JSON envelope.

  Point your client's MCP config at command \"ghostie\" with args [\"mcp\", \"serve\"].

EXIT CODES: 0 success · 1 failure · 2 bad arguments
";

const HELP_PROVENANCE: &str = "\
ghostie provenance <memory-id>
ghostie provenance verify

  Every memory write appends a deterministic, hash-chained record to
  <store>/.provenance/log.jsonl, so a memory's origin is verifiable and
  tamper-evident. The log syncs with your memories (it is the evidence), unlike
  the rebuildable .index/.

  <memory-id>   show that memory's lineage: each record's seq, event
                (created | updated | captured), content hash, and provenance
                (source, harness, core)
  verify        replay the whole chain and report INTACT, or the first BROKEN
                link by seq. Detects an edited log record (the entry hash no
                longer matches) and an edited memory file (its bytes no longer
                match the last recorded content hash)

EXAMPLES
  ghostie provenance fact-configs-live-in-etc-1
  ghostie provenance verify --json

EXIT CODES: 0 success (INTACT or a clean show) · 1 BROKEN chain or failure · 2 bad arguments
";

fn help(rest: &[String], json_mode: bool) -> CliOutput {
    let text = match rest.first().map(String::as_str) {
        Some("setup") => HELP_SETUP,
        Some("remember") => HELP_REMEMBER,
        Some("recall") => HELP_RECALL,
        Some("list") => HELP_LIST,
        Some("capture") => HELP_CAPTURE,
        Some("sync") => HELP_SYNC,
        Some("hook") => HELP_HOOK,
        Some("mcp") => HELP_MCP,
        Some("provenance") => HELP_PROVENANCE,
        _ => HELP,
    };
    let mut out = CliOutput::default();
    if json_mode {
        let envelope = Value::Object(vec![
            ("ok".to_string(), Value::Bool(true)),
            ("command".to_string(), Value::string("help")),
            (
                "data".to_string(),
                Value::Object(vec![("text".to_string(), Value::string(text))]),
            ),
            ("warnings".to_string(), Value::Array(vec![])),
        ]);
        out.stdout = envelope.emit();
        out.stdout.push('\n');
    } else {
        out.stdout = text.to_string();
    }
    out
}

fn version(json_mode: bool) -> CliOutput {
    let v = env!("CARGO_PKG_VERSION");
    let mut out = CliOutput::default();
    if json_mode {
        let envelope = Value::Object(vec![
            ("ok".to_string(), Value::Bool(true)),
            ("command".to_string(), Value::string("version")),
            (
                "data".to_string(),
                Value::Object(vec![("version".to_string(), Value::string(v))]),
            ),
            ("warnings".to_string(), Value::Array(vec![])),
        ]);
        out.stdout = envelope.emit();
        out.stdout.push('\n');
    } else {
        out.stdout = format!("ghostie {v}\n");
    }
    out
}

/// Hidden robot verb: the machine-readable subcommand list the gate's
/// robot-mode audit iterates. Not advertised in help.
fn subcommands_verb(json_mode: bool) -> CliOutput {
    let mut out = CliOutput::default();
    if json_mode {
        let envelope = Value::Object(vec![
            ("ok".to_string(), Value::Bool(true)),
            ("command".to_string(), Value::string("_subcommands")),
            (
                "data".to_string(),
                Value::Object(vec![(
                    "subcommands".to_string(),
                    Value::Array(SUBCOMMANDS.iter().map(|s| Value::string(*s)).collect()),
                )]),
            ),
            ("warnings".to_string(), Value::Array(vec![])),
        ]);
        out.stdout = envelope.emit();
        out.stdout.push('\n');
    } else {
        for s in SUBCOMMANDS {
            out.stdout.push_str(s);
            out.stdout.push('\n');
        }
    }
    out
}

// ---------- remember ----------

fn cmd_remember(
    store: &Store,
    rest: &[String],
    stdin_body: Option<&str>,
    _json_mode: bool,
) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut mtype: Option<MemoryType> = None;
    let mut title: Option<String> = None;
    let mut body = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut supersedes: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut core: Option<String> = None;
    let mut rationale: Option<String> = None;
    let mut scope: Option<String> = None;
    let mut no_redact = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            // Escape hatch: store content verbatim, skipping the secret
            // redaction that guards the write path by default.
            "--no-redact" => no_redact = true,
            "--type" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--type requires a value".to_string()))?;
                if v == "session-summary" {
                    return Err(usage(
                        "session summaries come from `ghostie capture`, not remember \
                         (provenance: the source field is capture's job)"
                            .to_string(),
                    ));
                }
                mtype = Some(MemoryType::parse(v).ok_or_else(|| {
                    usage(format!("unknown type '{v}' (fact | decision | rule)"))
                })?);
            }
            "--body" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--body requires text or '-'".to_string()))?;
                if v == "-" {
                    body = match stdin_body {
                        Some(s) => s.to_string(),
                        None => {
                            let mut buf = String::new();
                            use std::io::Read;
                            std::io::stdin()
                                .read_to_string(&mut buf)
                                .map_err(|e| Error::Io {
                                    context: "reading body from stdin".to_string(),
                                    path: "<stdin>".to_string(),
                                    source: e,
                                })?;
                            buf
                        }
                    };
                } else {
                    body = v.clone();
                }
            }
            "--tags" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--tags requires a,b,c".to_string()))?;
                tags = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "--link" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--link requires a memory id".to_string()))?;
                if !looks_like_memory_id(v) {
                    return Err(usage(format!(
                        "'{v}' does not look like a memory id (<type>-<slug>-<n>)"
                    )));
                }
                links.push(v.clone());
            }
            "--supersedes" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--supersedes requires a memory id".to_string()))?;
                supersedes = Some(v.clone());
            }
            "--harness" => {
                i += 1;
                harness = Some(
                    rest.get(i)
                        .ok_or_else(|| {
                            usage("--harness requires a value (e.g. claude-code)".to_string())
                        })?
                        .clone(),
                );
            }
            "--core" => {
                i += 1;
                core = Some(
                    rest.get(i)
                        .ok_or_else(|| {
                            usage("--core requires a value (e.g. opus-4.8)".to_string())
                        })?
                        .clone(),
                );
            }
            // `--why` is the ergonomic alias for `--rationale`: the reason
            // this memory is necessary, surfaced on the recall card.
            "--rationale" | "--why" => {
                i += 1;
                rationale = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--why requires text".to_string()))?
                        .clone(),
                );
            }
            "--scope" => {
                i += 1;
                scope = Some(
                    rest.get(i)
                        .ok_or_else(|| {
                            usage("--scope requires a value (global | project:<name>)".to_string())
                        })?
                        .clone(),
                );
            }
            a if a.starts_with('-') => {
                return Err(usage(format!("unknown flag '{a}' for remember")));
            }
            positional => {
                if title.is_some() {
                    return Err(usage(format!(
                        "unexpected extra argument {positional:?}; the title is one \
                         (quoted) argument"
                    )));
                }
                title = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let mtype =
        mtype.ok_or_else(|| usage("--type is required (fact | decision | rule)".to_string()))?;
    let title = title.ok_or_else(|| usage("a title is required".to_string()))?;
    if title.trim().is_empty() {
        return Err(usage("title must not be empty".to_string()));
    }
    let clock = resolve_clock()?;
    if no_redact {
        store.set_redaction(false);
    }
    let memory = store.create(
        &NewMemory {
            mtype: Some(mtype),
            title,
            tags,
            links,
            source: None,
            supersedes,
            harness,
            core,
            rationale,
            scope,
            body,
        },
        clock.as_ref(),
    )?;
    let path = format!("memories/{}.md", memory.id);
    Ok(CmdOk {
        data: Value::Object(vec![
            ("id".to_string(), Value::string(memory.id.clone())),
            ("path".to_string(), Value::string(path.clone())),
            ("type".to_string(), Value::string(memory.mtype.as_str())),
            ("title".to_string(), Value::string(memory.title.clone())),
            (
                "created".to_string(),
                Value::string(format_rfc3339_utc(memory.created)),
            ),
        ]),
        human: format!("{}  ({})\n", memory.id, path),
        warnings: Vec::new(),
    })
}

fn looks_like_memory_id(s: &str) -> bool {
    let mut parts = s.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    // session-summary ids start "session-summary-"; others "<type>-".
    let ok_type = matches!(first, "fact" | "decision" | "rule")
        || (first == "session" && s.starts_with("session-summary-"));
    ok_type
        && s.rsplit('-')
            .next()
            .is_some_and(|n| n.parse::<u64>().is_ok())
}

// ---------- recall ----------

fn cmd_recall(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut query: Option<String> = None;
    let mut opts = RecallOpts::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-k" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("-k requires a number".to_string()))?;
                opts.k = v
                    .parse::<usize>()
                    .map_err(|_| usage(format!("-k expects a positive integer, got '{v}'")))?;
                if opts.k == 0 {
                    return Err(usage("-k must be at least 1".to_string()));
                }
            }
            "--type" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--type requires a value".to_string()))?;
                opts.mtype = Some(MemoryType::parse(v).ok_or_else(|| {
                    usage(format!(
                        "unknown type '{v}' (fact | decision | rule | session-summary)"
                    ))
                })?);
            }
            "--tag" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--tag requires a value".to_string()))?;
                opts.tag = Some(v.clone());
            }
            "--scope" => {
                i += 1;
                let v = rest.get(i).ok_or_else(|| {
                    usage("--scope requires a value (global | project:<name>)".to_string())
                })?;
                opts.scope = Some(v.clone());
            }
            "--budget" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--budget requires a token count".to_string()))?;
                opts.budget_tokens = Some(v.parse::<usize>().map_err(|_| {
                    usage(format!("--budget expects a positive integer, got '{v}'"))
                })?);
            }
            "--diverse" => {
                opts.diversify = true;
            }
            "--no-rerank" => {
                opts.rerank = false;
            }
            a if a.starts_with('-') => {
                return Err(usage(format!("unknown flag '{a}' for recall")));
            }
            positional => {
                if query.is_some() {
                    return Err(usage(format!(
                        "unexpected extra argument {positional:?}; quote multi-word \
                         queries: ghostie recall \"two words\""
                    )));
                }
                query = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let query = query.ok_or_else(|| usage("a query is required".to_string()))?;
    let result = recall(store, &query, &opts)?;
    // Human rendering: one block per hit.
    let mut human = String::new();
    if result.hits.is_empty() {
        human.push_str("no memories matched\n");
    }
    for (rank, hit) in result.hits.iter().enumerate() {
        human.push_str(&format!(
            "{:>2}. {}  [{}]  score {}{}\n    {}\n",
            rank + 1,
            hit.id,
            hit.mtype.as_str(),
            render_score(hit.score_micros),
            hit.provenance_tag(),
            hit.title,
        ));
        // The card's why-line (rationale) when present — the reason this
        // memory matters, surfaced without pulling the body.
        if let Some(rationale) = &hit.rationale {
            human.push_str(&format!("    ↳ {rationale}\n"));
        }
        human.push_str(&format!("    {}\n", hit.why_line()));
    }
    Ok(CmdOk {
        data: result.to_json(),
        human,
        warnings: result.warnings.clone(),
    })
}

/// Render micros as a fixed-point decimal with exactly 3 fractional
/// digits (e.g. 1051671 -> "1.052"). Frozen by goldens.
fn render_score(micros: i64) -> String {
    let millis = (micros + 500) / 1000;
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

// ---------- list ----------

fn cmd_list(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut filter = ListFilter::default();
    let mut rebuild_index = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--type" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--type requires a value".to_string()))?;
                filter.mtype = Some(MemoryType::parse(v).ok_or_else(|| {
                    usage(format!(
                        "unknown type '{v}' (fact | decision | rule | session-summary)"
                    ))
                })?);
            }
            "--tag" => {
                i += 1;
                let v = rest
                    .get(i)
                    .ok_or_else(|| usage("--tag requires a value".to_string()))?;
                filter.tag = Some(v.clone());
            }
            "--rebuild-index" => rebuild_index = true,
            a if a.starts_with('-') => {
                return Err(usage(format!("unknown flag '{a}' for list")));
            }
            positional => {
                return Err(usage(format!(
                    "unexpected argument {positional:?} for list"
                )));
            }
        }
        i += 1;
    }
    let mut warnings = Vec::new();
    if rebuild_index {
        let (index, mut w) = crate::store::index::Index::build(store)?;
        warnings.append(&mut w);
        index.save(store.root())?;
    }
    let (memories, mut list_warnings) = store.list(&filter)?;
    warnings.append(&mut list_warnings);
    // Human: aligned columns computed from the actual rows (no terminal
    // probing — that would break byte-stability).
    let mut human = String::new();
    if memories.is_empty() {
        human.push_str("no memories in the store\n");
    } else {
        let id_w = memories.iter().map(|m| m.id.len()).max().unwrap_or(0);
        let ty_w = memories
            .iter()
            .map(|m| m.mtype.as_str().len())
            .max()
            .unwrap_or(0);
        for m in &memories {
            let date = &format_rfc3339_utc(m.created)[..10];
            let tags = if m.tags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", m.tags.join(", "))
            };
            human.push_str(&format!(
                "{:<id_w$}  {:<ty_w$}  {}  {}{}\n",
                m.id,
                m.mtype.as_str(),
                date,
                m.title,
                tags,
            ));
        }
    }
    let data = Value::Object(vec![
        (
            "memories".to_string(),
            Value::Array(
                memories
                    .iter()
                    .map(|m| {
                        Value::Object(vec![
                            ("id".to_string(), Value::string(m.id.clone())),
                            ("type".to_string(), Value::string(m.mtype.as_str())),
                            ("title".to_string(), Value::string(m.title.clone())),
                            (
                                "created".to_string(),
                                Value::string(format_rfc3339_utc(m.created)),
                            ),
                            (
                                "tags".to_string(),
                                Value::Array(m.tags.iter().map(Value::string).collect()),
                            ),
                            (
                                "path".to_string(),
                                Value::string(format!("memories/{}.md", m.id)),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("count".to_string(), Value::int(memories.len() as i64)),
    ]);
    Ok(CmdOk {
        data,
        human,
        warnings,
    })
}

/// Hidden maintenance verb: rewrite every readable memory in canonical
/// form (a no-op on an already-canonical store — the gate's
/// byte-stability step relies on exactly that). Useful to humans after a
/// messy hand-edit session; not advertised in help.
fn cmd_recanon(store: &Store) -> Result<CmdOk, Error> {
    let (memories, warnings) = store.list(&ListFilter::default())?;
    for m in &memories {
        store.update(m)?;
    }
    Ok(CmdOk {
        data: Value::Object(vec![(
            "rewritten".to_string(),
            Value::int(memories.len() as i64),
        )]),
        human: format!("rewrote {} memories in canonical form\n", memories.len()),
        warnings,
    })
}

/// `capture <transcript> [--harness h] [--core c]`: distill a session log into
/// memories (a session-summary plus any `MEMORY <type>:` markers).
fn cmd_capture(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut path: Option<String> = None;
    let mut harness: Option<String> = None;
    let mut core: Option<String> = None;
    let mut scope: Option<String> = None;
    let mut format: Option<capture::Format> = None;
    let mut latest: Option<String> = None;
    let mut no_redact = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--latest" => {
                i += 1;
                latest = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--latest requires a harness (codex)".to_string()))?
                        .clone(),
                );
            }
            // Escape hatch: ingest the transcript verbatim, skipping the
            // default secret redaction on the write path.
            "--no-redact" => no_redact = true,
            "--harness" => {
                i += 1;
                harness = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--harness requires a value".to_string()))?
                        .clone(),
                );
            }
            "--core" => {
                i += 1;
                core = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--core requires a value".to_string()))?
                        .clone(),
                );
            }
            "--scope" => {
                i += 1;
                scope = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--scope requires a value".to_string()))?
                        .clone(),
                );
            }
            "--format" => {
                i += 1;
                let v = rest.get(i).ok_or_else(|| {
                    usage("--format requires auto|claude-code|codex|generic".to_string())
                })?;
                format = capture::Format::parse(v)
                    .ok_or_else(|| usage(format!("unknown --format '{v}'")))?;
            }
            a if a.starts_with('-') => {
                return Err(usage(format!("unknown flag '{a}' for capture")));
            }
            positional => {
                if path.is_some() {
                    return Err(usage("capture takes one transcript path".to_string()));
                }
                path = Some(positional.to_string());
            }
        }
        i += 1;
    }
    // `--latest <harness>` resolves the path from the harness's own logs (Codex
    // notify carries no transcript path, so we find the newest rollout).
    let (path, format, harness) = if let Some(h) = latest {
        if path.is_some() {
            return Err(usage(
                "capture takes a path OR --latest, not both".to_string(),
            ));
        }
        match h.as_str() {
            "codex" => {
                let home = codex::codex_home()?;
                let found = codex::newest_rollout(&home).ok_or_else(|| Error::Invalid {
                    origin: home.join("sessions").display().to_string(),
                    message: "no Codex rollout (rollout-*.jsonl) found to capture".to_string(),
                })?;
                (
                    found.display().to_string(),
                    Some(capture::Format::Codex),
                    Some(harness.unwrap_or_else(|| "codex".to_string())),
                )
            }
            other => {
                return Err(usage(format!("--latest supports 'codex' (got '{other}')")));
            }
        }
    } else {
        let path = path.ok_or_else(|| usage("capture requires a transcript path".to_string()))?;
        (path, format, harness)
    };
    let clock = resolve_clock()?;
    if no_redact {
        store.set_redaction(false);
    }
    let created = capture::capture_file(
        store,
        &path,
        format,
        harness.as_deref(),
        core.as_deref(),
        scope.as_deref(),
        clock.as_ref(),
    )?;
    let items: Vec<Value> = created
        .iter()
        .map(|m| {
            Value::Object(vec![
                ("id".to_string(), Value::string(m.id.clone())),
                ("type".to_string(), Value::string(m.mtype.as_str())),
                ("title".to_string(), Value::string(m.title.clone())),
            ])
        })
        .collect();
    let mut human = format!("captured {} memory(ies):\n", created.len());
    for m in &created {
        human.push_str(&format!(
            "  {}  [{}]  {}\n",
            m.id,
            m.mtype.as_str(),
            m.title
        ));
    }
    Ok(CmdOk {
        data: Value::Object(vec![("created".to_string(), Value::Array(items))]),
        human,
        warnings: Vec::new(),
    })
}

/// `sync --init <remote>` wires the store to your own git remote; `sync`
/// commits, rebases in remote changes, and pushes.
fn cmd_sync(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut init: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--init" => {
                i += 1;
                init = Some(
                    rest.get(i)
                        .ok_or_else(|| usage("--init requires a git remote URL".to_string()))?
                        .clone(),
                );
            }
            a => return Err(usage(format!("unknown argument '{a}' for sync"))),
        }
        i += 1;
    }
    if let Some(remote) = init {
        sync::sync_init(store, &remote)?;
        return Ok(CmdOk {
            data: Value::Object(vec![
                ("initialized".to_string(), Value::Bool(true)),
                ("remote".to_string(), Value::string(remote.clone())),
            ]),
            human: format!("sync initialized against {remote}\n"),
            warnings: Vec::new(),
        });
    }
    let clock = resolve_clock()?;
    let out = sync::sync(store, clock.as_ref())?;
    Ok(CmdOk {
        data: Value::Object(vec![
            ("committed".to_string(), Value::Bool(out.committed)),
            ("pulled".to_string(), Value::Bool(out.pulled)),
            ("pushed".to_string(), Value::Bool(out.pushed)),
            ("branch".to_string(), Value::string(out.branch.clone())),
        ]),
        human: format!(
            "sync ok (branch {}): {}{}pushed\n",
            out.branch,
            if out.committed { "committed, " } else { "" },
            if out.pulled { "pulled, " } else { "" },
        ),
        warnings: Vec::new(),
    })
}

/// Read `--harness <value>` from a hook subcommand's args, defaulting to
/// `claude-code`. Shared by status/uninstall so they can target Codex.
fn harness_flag(rest: &[String], usage: &dyn Fn(String) -> Error) -> Result<String, Error> {
    let mut i = 1;
    while i < rest.len() {
        if rest[i] == "--harness" {
            i += 1;
            return Ok(rest
                .get(i)
                .ok_or_else(|| usage("--harness requires a value".to_string()))?
                .clone());
        }
        i += 1;
    }
    Ok("claude-code".to_string())
}

/// `hook run recall|capture` are the harness-invoked runners (payload on
/// stdin); `hook install|status|uninstall` manage the Claude Code settings or
/// the Codex `notify` program (`--harness codex`).
fn cmd_hook(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let sub = rest.first().map(String::as_str).ok_or_else(|| {
        usage("hook needs a subcommand: run | install | status | uninstall".to_string())
    })?;
    match sub {
        "run" => {
            let which = rest
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| usage("hook run needs: recall | capture".to_string()))?;
            // The runners read the harness payload from stdin.
            let mut stdin = String::new();
            use std::io::Read;
            std::io::stdin()
                .read_to_string(&mut stdin)
                .map_err(|e| Error::Io {
                    context: "reading hook payload from stdin".to_string(),
                    path: "<stdin>".to_string(),
                    source: e,
                })?;
            match which {
                "recall" => {
                    let mut budget = hook::DEFAULT_BUDGET;
                    let mut j = 2;
                    while j < rest.len() {
                        if rest[j] == "--budget" {
                            j += 1;
                            budget = rest
                                .get(j)
                                .and_then(|v| v.parse::<usize>().ok())
                                .ok_or_else(|| usage("--budget requires a number".to_string()))?;
                        }
                        j += 1;
                    }
                    let out = hook::run_recall(store, &stdin, budget)?;
                    Ok(CmdOk {
                        data: Value::Object(vec![(
                            "emitted".to_string(),
                            Value::Bool(!out.is_empty()),
                        )]),
                        human: out,
                        warnings: Vec::new(),
                    })
                }
                "capture" => {
                    let do_sync = rest.iter().any(|a| a == "--sync");
                    let clock = resolve_clock()?;
                    // Codex path: no transcript in the event; find + capture the
                    // newest rollout. Codex appends the event JSON as the last
                    // argv arg, so it lands as a trailing positional here.
                    let msg = if rest.iter().any(|a| a == "--codex-notify") {
                        let notify_arg = rest
                            .iter()
                            .skip(2)
                            .rfind(|a| !a.starts_with("--"))
                            .map(String::as_str);
                        hook::run_capture_codex_notify(
                            store,
                            notify_arg,
                            &stdin,
                            do_sync,
                            clock.as_ref(),
                        )?
                    } else {
                        hook::run_capture(store, &stdin, do_sync, clock.as_ref())?
                    };
                    Ok(CmdOk {
                        data: Value::Object(vec![(
                            "status".to_string(),
                            Value::string(msg.clone()),
                        )]),
                        human: format!("{msg}\n"),
                        warnings: Vec::new(),
                    })
                }
                other => Err(usage(format!("unknown hook runner '{other}'"))),
            }
        }
        "install" => {
            let mut budget = hook::DEFAULT_BUDGET;
            let mut do_sync = false;
            let mut harness = "claude-code".to_string();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--budget" => {
                        i += 1;
                        budget = rest
                            .get(i)
                            .and_then(|v| v.parse::<usize>().ok())
                            .ok_or_else(|| usage("--budget requires a number".to_string()))?;
                    }
                    "--sync" => do_sync = true,
                    "--harness" => {
                        i += 1;
                        harness = rest
                            .get(i)
                            .ok_or_else(|| usage("--harness requires a value".to_string()))?
                            .clone();
                    }
                    a => return Err(usage(format!("unknown flag '{a}' for hook install"))),
                }
                i += 1;
            }
            // Codex: auto-wire its `notify` program in ~/.codex/config.toml so
            // each completed turn captures the just-finished rollout. If a
            // foreign or multi-line notify is present we refuse to clobber it
            // and hand back the exact line to paste.
            if harness == "codex" {
                let home = codex::codex_home()?;
                let cfg = codex::config_path(&home);
                let argv = codex::notify_argv(store.root(), do_sync);
                let rep = codex::install_notify_at(&cfg, &argv)?;
                let human = if rep.applied {
                    format!(
                        "installed Codex notify capture in {}{}\nstart a new Codex session to activate\n",
                        rep.path.display(),
                        if rep.backed_up {
                            " (backup written)"
                        } else {
                            ""
                        },
                    )
                } else {
                    format!(
                        "Codex already has a `notify` configured in {}; Codex allows only one.\n\
                         Not overwriting it. To enable capture, merge this line yourself:\n  {}\n",
                        rep.path.display(),
                        rep.manual_line.clone().unwrap_or_default(),
                    )
                };
                return Ok(CmdOk {
                    data: Value::Object(vec![
                        ("harness".to_string(), Value::string("codex")),
                        (
                            "config".to_string(),
                            Value::string(rep.path.display().to_string()),
                        ),
                        ("applied".to_string(), Value::Bool(rep.applied)),
                        ("backed_up".to_string(), Value::Bool(rep.backed_up)),
                        (
                            "notify_line".to_string(),
                            Value::string(rep.manual_line.clone().unwrap_or_default()),
                        ),
                    ]),
                    human,
                    warnings: Vec::new(),
                });
            }
            // Turnkey auto-wiring exists for Claude Code (its settings.json hook
            // schema is stable and verifiable). The runners themselves are
            // harness-neutral, so any harness with a pre-prompt / session-end
            // hook can call them; we print exactly how rather than guess at a
            // config format we cannot verify.
            if harness != "claude-code" {
                let sync_flag = if do_sync { " --sync" } else { "" };
                let human = format!(
                    "No turnkey installer for '{harness}' yet (only claude-code).\n\
                     The runners are harness-neutral. Wire these into {harness}'s hooks:\n\
                     pre-prompt  ->  ghostie --store {} hook run recall --budget {budget}\n\
                     session-end ->  ghostie --store {} hook run capture{sync_flag}\n\
                     capture also works by hand for any harness: ghostie capture <transcript> --harness {harness}\n",
                    store.root().display(),
                    store.root().display(),
                );
                return Ok(CmdOk {
                    data: Value::Object(vec![
                        ("harness".to_string(), Value::string(harness)),
                        ("turnkey".to_string(), Value::Bool(false)),
                        (
                            "recall_command".to_string(),
                            Value::string(format!(
                                "ghostie --store {} hook run recall --budget {budget}",
                                store.root().display()
                            )),
                        ),
                        (
                            "capture_command".to_string(),
                            Value::string(format!(
                                "ghostie --store {} hook run capture{sync_flag}",
                                store.root().display()
                            )),
                        ),
                    ]),
                    human,
                    warnings: Vec::new(),
                });
            }
            let settings = hook::claude_settings_path()?;
            let rep = hook::install_at(&settings, store.root(), budget, do_sync)?;
            Ok(CmdOk {
                data: Value::Object(vec![
                    (
                        "settings".to_string(),
                        Value::string(rep.path.display().to_string()),
                    ),
                    ("backed_up".to_string(), Value::Bool(rep.backed_up)),
                ]),
                human: format!(
                    "installed recall + capture hooks in {}{}\nrestart Claude Code (or open a new session) to activate\n",
                    rep.path.display(),
                    if rep.backed_up {
                        " (backup written)"
                    } else {
                        ""
                    },
                ),
                warnings: Vec::new(),
            })
        }
        "status" => {
            let harness = harness_flag(rest, &usage)?;
            if harness == "codex" {
                let cfg = codex::config_path(&codex::codex_home()?);
                let on = codex::status_notify_at(&cfg);
                return Ok(CmdOk {
                    data: Value::Object(vec![
                        ("harness".to_string(), Value::string("codex")),
                        ("capture".to_string(), Value::Bool(on)),
                        (
                            "config".to_string(),
                            Value::string(cfg.display().to_string()),
                        ),
                    ]),
                    human: format!(
                        "codex notify capture: {}\n",
                        if on { "installed" } else { "not installed" }
                    ),
                    warnings: Vec::new(),
                });
            }
            let settings = hook::claude_settings_path()?;
            let (recall_on, capture_on) = hook::status_at(&settings)?;
            Ok(CmdOk {
                data: Value::Object(vec![
                    ("recall".to_string(), Value::Bool(recall_on)),
                    ("capture".to_string(), Value::Bool(capture_on)),
                ]),
                human: format!(
                    "recall-on-prompt: {}\ncapture-on-end:   {}\n",
                    if recall_on {
                        "installed"
                    } else {
                        "not installed"
                    },
                    if capture_on {
                        "installed"
                    } else {
                        "not installed"
                    },
                ),
                warnings: Vec::new(),
            })
        }
        "uninstall" => {
            let harness = harness_flag(rest, &usage)?;
            if harness == "codex" {
                let cfg = codex::config_path(&codex::codex_home()?);
                let removed = codex::uninstall_notify_at(&cfg)?;
                return Ok(CmdOk {
                    data: Value::Object(vec![
                        ("harness".to_string(), Value::string("codex")),
                        ("removed".to_string(), Value::Bool(removed)),
                    ]),
                    human: format!(
                        "{}\n",
                        if removed {
                            "removed ghostie's Codex notify line"
                        } else {
                            "no ghostie Codex notify line to remove"
                        }
                    ),
                    warnings: Vec::new(),
                });
            }
            let settings = hook::claude_settings_path()?;
            let removed = hook::uninstall_at(&settings)?;
            Ok(CmdOk {
                data: Value::Object(vec![("removed".to_string(), Value::int(removed as i64))]),
                human: format!("removed {removed} ghostie hook entr(ies)\n"),
                warnings: Vec::new(),
            })
        }
        other => Err(usage(format!("unknown hook subcommand '{other}'"))),
    }
}

/// `mcp serve` runs the MCP stdio server (a long-running JSON-RPC loop that
/// reads stdin and writes responses directly); bare `mcp` (and `mcp --json`)
/// prints a one-shot manifest envelope (server identity + tool list) and
/// exits, so the verb honors the robot-mode contract without blocking.
fn cmd_mcp(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    match rest.first().map(String::as_str) {
        Some("serve") => {
            // The server writes JSON-RPC responses to stdout itself and returns
            // on stdin EOF; nothing is added to the envelope path.
            crate::mcp::serve(store)?;
            Ok(CmdOk {
                data: Value::Object(vec![("served".to_string(), Value::Bool(true))]),
                human: String::new(),
                warnings: Vec::new(),
            })
        }
        None => {
            // Manifest: server identity + the tool catalog, one-shot.
            let data = crate::mcp::manifest_data();
            let tool_names: Vec<String> = data
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let human = format!(
                "ghostie MCP server {} (protocol {})\ntools: {}\nrun `ghostie mcp serve` and point your MCP client at it\n",
                crate::mcp::server_version(),
                crate::mcp::PROTOCOL_VERSION,
                tool_names.join(", "),
            );
            Ok(CmdOk {
                data,
                human,
                warnings: Vec::new(),
            })
        }
        Some(other) => Err(usage(format!(
            "unknown mcp subcommand '{other}' (serve, or no argument for the manifest)"
        ))),
    }
}

/// `provenance <id>` shows a memory's hash-chained lineage; `provenance verify`
/// replays the whole chain and reports INTACT (exit 0) or the first BROKEN link
/// (exit 1, so a script or CI gate fails on tampering). Robot `--json` on both.
fn cmd_provenance(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    use crate::provenance::{self, VerifyReport};
    let usage = |message: String| Error::Usage { message };
    match rest.first().map(String::as_str) {
        None => Err(usage(
            "provenance needs a memory id, or `verify` to replay the chain".to_string(),
        )),
        Some("verify") => {
            if rest.len() > 1 {
                return Err(usage(format!(
                    "provenance verify takes no arguments (got {:?})",
                    &rest[1..]
                )));
            }
            match provenance::verify(store.root())? {
                VerifyReport::Intact { entries, memories } => Ok(CmdOk {
                    data: Value::Object(vec![
                        ("status".to_string(), Value::string("intact")),
                        ("entries".to_string(), Value::int(entries as i64)),
                        ("memories".to_string(), Value::int(memories as i64)),
                    ]),
                    human: format!(
                        "provenance INTACT: {entries} record(s) across {memories} memory(ies)\n"
                    ),
                    warnings: Vec::new(),
                }),
                // A broken chain is an operational failure (exit 1): scripts and
                // the CI gate must fail on tampering, not read a success envelope.
                VerifyReport::Broken { seq, reason } => Err(Error::Invalid {
                    origin: provenance::log_path(store.root()).display().to_string(),
                    message: format!("provenance BROKEN at seq {seq}: {reason}"),
                }),
            }
        }
        Some(flag) if flag.starts_with('-') => {
            Err(usage(format!("unknown flag '{flag}' for provenance")))
        }
        Some(id) => {
            let entries = provenance::lineage(store.root(), id)?;
            let items: Vec<Value> = entries.iter().map(provenance::Entry::to_json).collect();
            let mut human = if entries.is_empty() {
                format!("no provenance recorded for {id}\n")
            } else {
                format!("provenance for {id} ({} record(s)):\n", entries.len())
            };
            for e in &entries {
                human.push_str(&format!(
                    "  seq {:>3}  {:<9}  content {}  entry {}  {}\n",
                    e.seq,
                    e.event.as_str(),
                    e.content_hash,
                    e.entry_hash,
                    format_rfc3339_utc(e.created),
                ));
            }
            Ok(CmdOk {
                data: Value::Object(vec![
                    ("memory_id".to_string(), Value::string(id)),
                    ("entries".to_string(), Value::Array(items)),
                    ("count".to_string(), Value::int(entries.len() as i64)),
                ]),
                human,
                warnings: Vec::new(),
            })
        }
    }
}

/// `setup [<git-remote>] [--budget N]`: the one button. Wires your own remote
/// (when given), installs the recall + capture hooks, and does the first push,
/// so cross-provider memory just works after a single command.
fn cmd_setup(store: &Store, rest: &[String], _json_mode: bool) -> Result<CmdOk, Error> {
    let usage = |message: String| Error::Usage { message };
    let mut remote: Option<String> = None;
    let mut budget = hook::DEFAULT_BUDGET;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--budget" => {
                i += 1;
                budget = rest
                    .get(i)
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| usage("--budget requires a number".to_string()))?;
            }
            a if a.starts_with('-') => {
                return Err(usage(format!("unknown flag '{a}' for setup")));
            }
            positional => {
                if remote.is_some() {
                    return Err(usage("setup takes one git remote".to_string()));
                }
                remote = Some(positional.to_string());
            }
        }
        i += 1;
    }
    let do_sync = remote.is_some();
    let warnings: Vec<Warning> = Vec::new();
    let mut steps: Vec<String> = Vec::new();

    if let Some(r) = &remote {
        sync::sync_init(store, r)?;
        steps.push(format!("sync wired to {r}"));
    }
    let settings = hook::claude_settings_path()?;
    let rep = hook::install_at(&settings, store.root(), budget, do_sync)?;
    steps.push(format!(
        "recall + capture hooks installed{}",
        if rep.backed_up {
            " (settings backed up)"
        } else {
            ""
        }
    ));
    let mut synced = false;
    if do_sync {
        let clock = resolve_clock()?;
        // The hooks are installed by now, but setup PROMISES a first push. If
        // it fails (auth, network, a bad remote, or a conflict), fail loudly
        // with the real cause and exit code rather than reporting success and
        // leaving the user believing their memory is backed up. Conflicts keep
        // exit code 3.
        sync::sync(store, clock.as_ref()).map_err(|e| match e {
            Error::Conflict { .. } => e,
            other => Error::Invalid {
                origin: "setup".to_string(),
                message: format!(
                    "hooks were installed, but the initial sync failed: {other}. \
                     Fix the remote and run `ghostie sync`."
                ),
            },
        })?;
        synced = true;
        steps.push("initial sync pushed".to_string());
    }

    let mut human = String::from(if do_sync {
        "ghostie is set up.\n"
    } else {
        "ghostie is set up (local only).\n"
    });
    for s in &steps {
        human.push_str(&format!("  done: {s}\n"));
    }
    if !do_sync {
        human.push_str("add cross-device sync anytime: ghostie setup <your-git-remote>\n");
    }
    human.push_str("restart Claude Code (or open a new session) to activate\n");

    Ok(CmdOk {
        data: Value::Object(vec![
            (
                "remote".to_string(),
                match &remote {
                    Some(r) => Value::string(r.clone()),
                    None => Value::Null,
                },
            ),
            (
                "settings".to_string(),
                Value::string(rep.path.display().to_string()),
            ),
            ("hooks_installed".to_string(), Value::Bool(true)),
            ("synced".to_string(), Value::Bool(synced)),
        ]),
        human,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    fn store_args(tmp: &TempDir) -> Vec<String> {
        vec!["--store".to_string(), tmp.path().display().to_string()]
    }

    fn run_in(tmp: &TempDir, args: &[&str], stdin: Option<&str>) -> CliOutput {
        let mut full = store_args(tmp);
        full.extend(s(args));
        execute(&full, stdin)
    }

    // ---------- framework (bead 4.1) ----------

    #[test]
    fn version_envelope_golden() {
        let out = execute(&s(&["version", "--json"]), None);
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            out.stdout,
            format!(
                "{{\"ok\":true,\"command\":\"version\",\"data\":{{\"version\":\"{}\"}},\"warnings\":[]}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        let out = execute(&s(&["frobnicate"]), None);
        assert_eq!(out.exit_code, 2);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("unknown command"), "{}", out.stderr);
    }

    #[test]
    fn unknown_flag_is_usage_error_with_json_envelope() {
        let tmp = TempDir::new("cli-badflag");
        let out = run_in(&tmp, &["list", "--wat", "--json"], None);
        assert_eq!(out.exit_code, 2);
        assert!(out.stdout.starts_with("{\"ok\":false,\"command\":\"list\""));
        assert!(out.stdout.contains("\"kind\":\"usage\""), "{}", out.stdout);
        // Exactly one JSON line on stdout, parseable by our own parser.
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        assert_eq!(doc.get("ok").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn missing_flag_value_is_usage_error() {
        let out = execute(&s(&["recall", "-k"]), None);
        assert_eq!(out.exit_code, 2);
        let out = execute(&s(&["--store"]), None);
        assert_eq!(out.exit_code, 2);
    }

    #[test]
    fn store_flag_beats_env() {
        // Flag form and = form both parse; the flag path is used (the env
        // path would require env mutation, which tests avoid — precedence
        // over env is covered by the e2e suite with real processes).
        let tmp = TempDir::new("cli-precedence");
        let out = run_in(&tmp, &["list"], None);
        assert_eq!(out.exit_code, 0);
        let g = Globals::parse(&s(&["--store=/tmp/x", "list"])).unwrap();
        assert_eq!(g.store_root, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn no_command_is_usage_error() {
        let out = execute(&[], None);
        assert_eq!(out.exit_code, 2);
        let out = execute(&s(&["--json"]), None);
        assert_eq!(out.exit_code, 2);
        assert!(out.stdout.contains("\"ok\":false"));
    }

    #[test]
    fn help_covers_all_subcommands_and_exit_codes() {
        let out = execute(&s(&["help"]), None);
        assert_eq!(out.exit_code, 0);
        for sc in SUBCOMMANDS {
            assert!(out.stdout.contains(sc), "help must mention {sc}");
        }
        assert!(out.stdout.contains("EXIT CODES"));
        for sub in ["remember", "recall", "list"] {
            let out = execute(&s(&["help", sub]), None);
            assert!(out.stdout.contains("EXAMPLES"), "{sub} help has examples");
        }
    }

    #[test]
    fn subcommands_verb_lists_the_dispatcher_truth() {
        let out = execute(&s(&["_subcommands", "--json"]), None);
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        let listed: Vec<&str> = doc
            .get("data")
            .and_then(|d| d.get("subcommands"))
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(listed, SUBCOMMANDS.to_vec());
    }

    // ---------- remember (bead 4.2) ----------

    #[test]
    fn remember_creates_and_reports() {
        let tmp = TempDir::new("cli-remember");
        let out = run_in(
            &tmp,
            &[
                "remember",
                "--type",
                "rule",
                "Always run verify.sh before commit",
                "--tags",
                "ci, discipline ,",
                "--body",
                "The gate is verify.sh.",
            ],
            None,
        );
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert!(
            out.stdout
                .starts_with("rule-always-run-verify-sh-before-commit-1"),
            "{}",
            out.stdout
        );
        // Tags: comma-split, trimmed, empties dropped, order preserved.
        let text = std::fs::read_to_string(
            tmp.path()
                .join("memories/rule-always-run-verify-sh-before-commit-1.md"),
        )
        .unwrap();
        assert!(text.contains("tags: [ci, discipline]"), "{text}");
    }

    #[test]
    fn remember_json_envelope() {
        let tmp = TempDir::new("cli-remember-json");
        let out = run_in(
            &tmp,
            &[
                "remember",
                "--type",
                "fact",
                "Configs live in etc",
                "--json",
            ],
            None,
        );
        assert_eq!(out.exit_code, 0);
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        assert_eq!(doc.get("ok").and_then(Value::as_bool), Some(true));
        let data = doc.get("data").unwrap();
        assert_eq!(
            data.get("id").and_then(Value::as_str),
            Some("fact-configs-live-in-etc-1")
        );
        assert_eq!(
            data.get("path").and_then(Value::as_str),
            Some("memories/fact-configs-live-in-etc-1.md")
        );
        assert!(data.get("created").is_some());
    }

    #[test]
    fn remember_body_from_stdin() {
        let tmp = TempDir::new("cli-remember-stdin");
        let out = run_in(
            &tmp,
            &["remember", "--type", "fact", "stdin body", "--body", "-"],
            Some("line one\nline two\n"),
        );
        assert_eq!(out.exit_code, 0);
        let text =
            std::fs::read_to_string(tmp.path().join("memories/fact-stdin-body-1.md")).unwrap();
        assert!(text.ends_with("---\nline one\nline two\n"), "{text}");
    }

    #[test]
    fn remember_rejects_session_summary_pointing_at_capture() {
        let tmp = TempDir::new("cli-remember-ss");
        let out = run_in(
            &tmp,
            &["remember", "--type", "session-summary", "nope"],
            None,
        );
        assert_eq!(out.exit_code, 2);
        assert!(out.stderr.contains("capture"), "{}", out.stderr);
    }

    #[test]
    fn remember_rejections() {
        let tmp = TempDir::new("cli-remember-bad");
        for (args, needle) in [
            (vec!["remember", "TitleOnly"], "--type is required"),
            (vec!["remember", "--type", "opinion", "t"], "unknown type"),
            (vec!["remember", "--type", "fact"], "title is required"),
            (
                vec!["remember", "--type", "fact", "t", "extra"],
                "extra argument",
            ),
            (
                vec!["remember", "--type", "fact", "t", "--link", "not an id"],
                "does not look like a memory id",
            ),
        ] {
            let out = run_in(&tmp, &args, None);
            assert_eq!(out.exit_code, 2, "args {args:?}");
            assert!(out.stderr.contains(needle), "args {args:?}: {}", out.stderr);
        }
    }

    #[test]
    fn remember_created_file_is_byte_stable() {
        let tmp = TempDir::new("cli-remember-stable");
        let out = run_in(
            &tmp,
            &[
                "remember",
                "--type",
                "decision",
                "Chose plain files",
                "--body",
                "b",
            ],
            None,
        );
        assert_eq!(out.exit_code, 0);
        let path = tmp.path().join("memories/decision-chose-plain-files-1.md");
        let bytes = std::fs::read_to_string(&path).unwrap();
        let doc = crate::store::frontmatter::parse(&bytes, "<t>").unwrap();
        assert_eq!(doc.serialize(), bytes, "canonical on first write");
    }

    // ---------- recall (bead 4.3) ----------

    fn seeded(tmp: &TempDir) {
        for (ty, title, tags, body) in [
            (
                "rule",
                "Sync branch is sync never main",
                "git,sync",
                "Push to the sync branch.",
            ),
            (
                "decision",
                "Chose fixed-point over floats",
                "determinism",
                "Floats round differently.",
            ),
            (
                "fact",
                "Configs live in etc",
                "layout",
                "All configs live in etc/.",
            ),
        ] {
            let out = run_in(
                tmp,
                &[
                    "remember", "--type", ty, title, "--tags", tags, "--body", body,
                ],
                None,
            );
            assert_eq!(out.exit_code, 0, "{}", out.stderr);
        }
    }

    #[test]
    fn recall_human_golden() {
        let tmp = TempDir::new("cli-recall-human");
        seeded(&tmp);
        let out = run_in(&tmp, &["recall", "which branch do we sync to"], None);
        assert_eq!(out.exit_code, 0);
        let first_line = out.stdout.lines().next().unwrap();
        assert!(
            first_line.starts_with(" 1. rule-sync-branch-is-sync-never-main-1  [rule]  score "),
            "{first_line}"
        );
        assert!(out.stdout.contains("why: "), "{}", out.stdout);
        assert!(
            out.stdout.contains("ignored"),
            "stopwords shown: {}",
            out.stdout
        );
    }

    #[test]
    fn recall_json_is_byte_stable_and_structured() {
        let tmp = TempDir::new("cli-recall-json");
        seeded(&tmp);
        let a = run_in(&tmp, &["recall", "sync branch", "--json"], None);
        let b = run_in(&tmp, &["recall", "sync branch", "--json"], None);
        assert_eq!(a.exit_code, 0);
        assert_eq!(a.stdout, b.stdout, "byte-stable robot output");
        let doc = crate::json::parse(a.stdout.trim_end()).unwrap();
        let hits = doc
            .get("data")
            .and_then(|d| d.get("hits"))
            .and_then(Value::as_array)
            .unwrap();
        assert!(!hits.is_empty());
        let why = hits[0].get("why").unwrap();
        assert!(
            !why.get("matched_terms")
                .and_then(Value::as_array)
                .unwrap()
                .is_empty(),
            "non-empty why in robot mode"
        );
    }

    #[test]
    fn recall_zero_hits_is_success() {
        let tmp = TempDir::new("cli-recall-zero");
        seeded(&tmp);
        let out = run_in(&tmp, &["recall", "kubernetes warp drive"], None);
        assert_eq!(out.exit_code, 0, "empty is an answer, not an error");
        assert_eq!(out.stdout, "no memories matched\n");
    }

    #[test]
    fn recall_flags_map_to_opts() {
        let tmp = TempDir::new("cli-recall-flags");
        seeded(&tmp);
        let out = run_in(
            &tmp,
            &[
                "recall",
                "sync configs floats",
                "-k",
                "1",
                "--type",
                "fact",
                "--json",
            ],
            None,
        );
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        let hits = doc
            .get("data")
            .and_then(|d| d.get("hits"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].get("type").and_then(Value::as_str), Some("fact"));
    }

    #[test]
    fn recall_unquoted_multiword_query_caught() {
        let tmp = TempDir::new("cli-recall-unquoted");
        seeded(&tmp);
        let out = run_in(&tmp, &["recall", "sync", "branch"], None);
        assert_eq!(out.exit_code, 2);
        assert!(out.stderr.contains("quote"), "{}", out.stderr);
    }

    #[test]
    fn recall_warning_surfaces_in_json() {
        let tmp = TempDir::new("cli-recall-warn");
        seeded(&tmp);
        std::fs::write(tmp.path().join("memories/broken.md"), "not a memory").unwrap();
        let out = run_in(&tmp, &["recall", "sync branch", "--json"], None);
        assert_eq!(out.exit_code, 0);
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        let warnings = doc.get("warnings").and_then(Value::as_array).unwrap();
        assert!(
            warnings.iter().any(|w| w
                .get("origin")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("broken.md")),
            "{}",
            out.stdout
        );
    }

    // ---------- list (bead 4.4) ----------

    #[test]
    fn list_human_golden_alignment() {
        let tmp = TempDir::new("cli-list-human");
        seeded(&tmp);
        let out = run_in(&tmp, &["list"], None);
        assert_eq!(out.exit_code, 0);
        let want = "\
decision-chose-fixed-point-over-floats-1  decision  2026-07-13  Chose fixed-point over floats  [determinism]
fact-configs-live-in-etc-1                fact      2026-07-13  Configs live in etc  [layout]
rule-sync-branch-is-sync-never-main-1     rule      2026-07-13  Sync branch is sync never main  [git, sync]
";
        // The created date depends on the test wall clock; compare shape
        // by masking the date column.
        let mask = |s: &str| {
            s.lines()
                .map(|l| {
                    let mut cols: Vec<&str> = l.split("  ").collect();
                    for c in cols.iter_mut() {
                        if c.len() == 10 && c.as_bytes()[4] == b'-' {
                            *c = "DATE";
                        }
                    }
                    cols.join("  ")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(mask(&out.stdout), mask(want));
    }

    #[test]
    fn list_json_and_filters_and_empty() {
        let tmp = TempDir::new("cli-list-json");
        let out = run_in(&tmp, &["list"], None);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "no memories in the store\n");
        let out = run_in(&tmp, &["list", "--json"], None);
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        assert_eq!(
            doc.get("data")
                .and_then(|d| d.get("count"))
                .and_then(Value::as_i64),
            Some(0)
        );
        seeded(&tmp);
        let out = run_in(&tmp, &["list", "--type", "rule", "--json"], None);
        let doc = crate::json::parse(out.stdout.trim_end()).unwrap();
        let memories = doc
            .get("data")
            .and_then(|d| d.get("memories"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(
            memories[0].get("path").and_then(Value::as_str),
            Some("memories/rule-sync-branch-is-sync-never-main-1.md"),
            "path included: the files being touchable is the product's stance"
        );
    }

    #[test]
    fn list_rebuild_index_verb() {
        let tmp = TempDir::new("cli-list-rebuild");
        seeded(&tmp);
        let out = run_in(&tmp, &["list", "--rebuild-index"], None);
        assert_eq!(out.exit_code, 0);
        assert!(
            crate::store::index::index_path(tmp.path()).exists(),
            "rebuild materializes the index"
        );
    }

    #[test]
    fn recanon_is_a_noop_on_a_canonical_store() {
        let tmp = TempDir::new("cli-recanon");
        seeded(&tmp);
        let before: Vec<(String, Vec<u8>)> = {
            let mut v: Vec<_> = std::fs::read_dir(tmp.path().join("memories"))
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().to_string(),
                        std::fs::read(e.path()).unwrap(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        let out = run_in(&tmp, &["_recanon"], None);
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("rewrote 3 memories"), "{}", out.stdout);
        let after: Vec<(String, Vec<u8>)> = {
            let mut v: Vec<_> = std::fs::read_dir(tmp.path().join("memories"))
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().to_string(),
                        std::fs::read(e.path()).unwrap(),
                    )
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(before, after, "recanon on canonical store is byte-noop");
    }

    #[test]
    fn list_corrupt_file_warns_but_lists_rest() {
        let tmp = TempDir::new("cli-list-corrupt");
        seeded(&tmp);
        std::fs::write(tmp.path().join("memories/broken.md"), "junk").unwrap();
        let out = run_in(&tmp, &["list"], None);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.lines().count(), 3, "good memories listed");
        assert!(out.stderr.contains("broken.md"), "{}", out.stderr);
        // --quiet silences human warnings.
        let quiet = run_in(&tmp, &["list", "--quiet"], None);
        assert!(quiet.stderr.is_empty());
    }
}
