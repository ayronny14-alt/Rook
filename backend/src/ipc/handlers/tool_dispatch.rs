// consolidated tool surface. 11 top-level tools, each with an `op` discriminator
// that maps to one of the original leaf handlers. descriptions are compact,
// errors tell the LLM how to call the tool correctly.

use crate::llm::types::{FunctionDefinition, ToolDefinition};
use serde_json::{json, Value};

struct Op {
    op: &'static str,
    inner: &'static str,
    sig: &'static str,  // compact param shape, e.g. "{query, limit?}"
    note: &'static str, // one-liner; empty string if none
    required: &'static [&'static str], // required params besides `op` itself
}

struct Tool {
    name: &'static str,
    hint: &'static str,
    ops: &'static [Op],
    // extra JSON schema fields beyond `op`. loose: all optional, handlers re-check types.
    // built from a template string so we don't hand-write the whole schema twice.
    extra_props: &'static str,
}

const MEMORY: Tool = Tool {
    name: "memory",
    hint: "long-term memory graph across sessions. search BEFORE asking the user to repeat context; store non-obvious facts worth recalling later.",
    ops: &[
        Op { op: "search",          inner: "search_memory",   sig: "{query, limit?}",     note: "ranked results, cheap",      required: &["query"] },
        Op { op: "store",           inner: "store_memory",    sig: "{content, category?}", note: "category: project|decision|architecture|bug|preference", required: &["content"] },
        Op { op: "store_user_fact", inner: "store_user_fact", sig: "{key, value}",         note: "global user profile",        required: &["key","value"] },
    ],
    extra_props: r#"
        "query": {"type":"string"},
        "limit": {"type":"integer"},
        "content": {"type":"string"},
        "category": {"type":"string"},
        "key": {"type":"string"},
        "value": {"type":"string"}
    "#,
};

const SCHEDULE: Tool = Tool {
    name: "schedule",
    hint: "scheduled tasks. propose starts status='proposed', user must approve.",
    ops: &[
        Op { op: "propose", inner: "propose_schedule", sig: "{name, cadence, prompt, output_channel?, why?}",
             note: "cadence: 'once YYYY-MM-DD HH:MM' | 'in 2h' | 'daily 09:00' | 'weekly mon 09:00' | 'every 15m' | 'cron 0 9 * * 1'",
             required: &["name","cadence","prompt"] },
        Op { op: "list",    inner: "list_schedules",   sig: "{}",     note: "",  required: &[] },
        Op { op: "cancel",  inner: "cancel_schedule",  sig: "{id}",   note: "",  required: &["id"] },
    ],
    extra_props: r#"
        "name": {"type":"string"},
        "cadence": {"type":"string"},
        "prompt": {"type":"string"},
        "output_channel": {"type":"string","enum":["notification","silent"]},
        "why": {"type":"string"},
        "id": {"type":"string"}
    "#,
};

const GIT: Tool = Tool {
    name: "git",
    hint: "git operations. commit ONLY when the user explicitly asks; never commit just because tests passed.",
    ops: &[
        Op { op: "status", inner: "git_status", sig: "{}",               note: "",            required: &[] },
        Op { op: "diff",   inner: "git_diff",   sig: "{file?}",          note: "",            required: &[] },
        Op { op: "log",    inner: "git_log",    sig: "{count?}",         note: "default 10",  required: &[] },
        Op { op: "branch", inner: "git_branch", sig: "{}",               note: "",            required: &[] },
        Op { op: "commit", inner: "git_commit", sig: "{message, files}", note: "files: array of paths; use ['.'] to stage all", required: &["message","files"] },
    ],
    extra_props: r#"
        "file": {"type":"string"},
        "count": {"type":"integer"},
        "message": {"type":"string"},
        "files": {"type":"array","items":{"type":"string"}}
    "#,
};

const UI: Tool = Tool {
    name: "ui",
    hint: "Windows UIA desktop automation (not for web pages - use browser). call op=snapshot BEFORE every click so ids match the current screen.",
    ops: &[
        Op { op: "snapshot", inner: "ui_snapshot",     sig: "{window?, include_offscreen?}",
             note: "returns element tree + last 8 UI actions", required: &[] },
        Op { op: "click",    inner: "ui_click",        sig: "{element_id}",         note: "id from most recent snapshot", required: &["element_id"] },
        Op { op: "type",     inner: "ui_type",         sig: "{element_id, text}",   note: "",                             required: &["element_id","text"] },
        Op { op: "focus",    inner: "ui_focus_window", sig: "{title}",              note: "substring match; snapshot again after focusing", required: &["title"] },
        Op { op: "find",     inner: "ui_find",         sig: "{query}",              note: "search last snapshot by element name",           required: &["query"] },
    ],
    extra_props: r#"
        "window": {"type":"string"},
        "include_offscreen": {"type":"boolean"},
        "element_id": {"type":"string"},
        "text": {"type":"string"},
        "title": {"type":"string"},
        "query": {"type":"string"}
    "#,
};

const BROWSER: Tool = Tool {
    name: "browser",
    hint: "headless browser via CDP. use for web pages that need JS; prefer web.fetch for static text.",
    ops: &[
        Op { op: "navigate",   inner: "browser_navigate",   sig: "{url}",            note: "returns page text",   required: &["url"] },
        Op { op: "click",      inner: "browser_click",      sig: "{selector}",       note: "CSS selector",        required: &["selector"] },
        Op { op: "type",       inner: "browser_type",       sig: "{selector, text}", note: "",                    required: &["selector","text"] },
        Op { op: "eval",       inner: "browser_evaluate",   sig: "{js}",             note: "returns result",      required: &["js"] },
        Op { op: "screenshot", inner: "browser_screenshot", sig: "{full_page?}",     note: "base64 PNG",          required: &[] },
    ],
    extra_props: r#"
        "url": {"type":"string"},
        "selector": {"type":"string"},
        "text": {"type":"string"},
        "js": {"type":"string"},
        "full_page": {"type":"boolean"}
    "#,
};

const SHELL: Tool = Tool {
    name: "shell",
    hint: "persistent named shells. cwd+env survive across calls. default shell='default'.",
    ops: &[
        Op { op: "exec",  inner: "shell_exec",   sig: "{command, shell?, run_in_background?, timeout_secs?}",
             note: "sync timeout 45s default; run_in_background=true for dev servers, read output via op=read",
             required: &["command"] },
        Op { op: "spawn", inner: "shell_spawn",  sig: "{name}",            note: "new named shell; errors if already exists, kill first", required: &["name"] },
        Op { op: "read",  inner: "shell_read",   sig: "{shell?, clear?}",  note: "reads background buffer",            required: &[] },
        Op { op: "kill",  inner: "shell_kill",   sig: "{name}",            note: "",                                    required: &["name"] },
        Op { op: "list",  inner: "shell_list",   sig: "{}",                note: "",                                    required: &[] },
    ],
    extra_props: r#"
        "command": {"type":"string"},
        "shell": {"type":"string"},
        "run_in_background": {"type":"boolean"},
        "timeout_secs": {"type":"integer"},
        "name": {"type":"string"},
        "clear": {"type":"boolean"}
    "#,
};

const LSP: Tool = Tool {
    name: "lsp",
    hint: "read-only code intelligence at a cursor (definition, references, type info). For rename/refactor use semantic instead. 0-based line/character.",
    ops: &[
        Op { op: "definition", inner: "lsp_definition", sig: "{path, line, character}", note: "",                    required: &["path","line","character"] },
        Op { op: "references", inner: "lsp_references", sig: "{path, line, character}", note: "",                    required: &["path","line","character"] },
        Op { op: "hover",      inner: "lsp_hover",      sig: "{path, line, character}", note: "type info + docs",    required: &["path","line","character"] },
    ],
    extra_props: r#"
        "path": {"type":"string"},
        "line": {"type":"integer"},
        "character": {"type":"integer"}
    "#,
};

const FS_READ: Tool = Tool {
    name: "fs_read",
    hint: "read-only file/search. use grep/search_in_file BEFORE read to locate relevant lines.",
    ops: &[
        Op {
            op: "read",
            inner: "file_read",
            sig: "{path, offset?, limit?}",
            note: "line-numbered output, limit default 2000",
            required: &["path"],
        },
        Op {
            op: "list",
            inner: "list_files",
            sig: "{path?, recursive?, pattern?}",
            note: "recursive depth 3",
            required: &[],
        },
        Op {
            op: "cwd",
            inner: "get_cwd",
            sig: "{}",
            note: "",
            required: &[],
        },
        Op {
            op: "chdir",
            inner: "change_dir",
            sig: "{path}",
            note: "",
            required: &["path"],
        },
        Op {
            op: "glob",
            inner: "glob",
            sig: "{pattern, path?}",
            note: "e.g. 'src/**/*.rs'",
            required: &["pattern"],
        },
        Op {
            op: "grep",
            inner: "grep",
            sig: "{pattern, path?, glob?, output?, case_insensitive?, max_results?}",
            note: "output: files|content|count (default files)",
            required: &["pattern"],
        },
        Op {
            op: "outline",
            inner: "outline_file",
            sig: "{path}",
            note: "symbols+line numbers; rs|js|ts|py|go",
            required: &["path"],
        },
        Op {
            op: "search_in_file",
            inner: "search_in_file",
            sig: "{path, pattern, context?, case_insensitive?}",
            note: "use when you already know the file",
            required: &["path", "pattern"],
        },
    ],
    extra_props: r#"
        "path": {"type":"string"},
        "offset": {"type":"integer"},
        "limit": {"type":"integer"},
        "recursive": {"type":"boolean"},
        "pattern": {"type":"string"},
        "glob": {"type":"string"},
        "output": {"type":"string","enum":["files","content","count"]},
        "case_insensitive": {"type":"boolean"},
        "max_results": {"type":"integer"},
        "context": {"type":"integer"}
    "#,
};

const FS_WRITE: Tool = Tool {
    name: "fs_write",
    hint: "mutating file ops. for rename/extract/refactor prefer semantic (it updates references). op=diff and op=undo work because write/edit/patch snapshot first.",
    ops: &[
        Op { op: "write", inner: "file_write",  sig: "{path, content}", note: "create or overwrite",                required: &["path","content"] },
        Op { op: "edit",  inner: "code_edit",   sig: "{path, action, search?, replace?, line?, content?}",
             note: "action: search_replace | insert_at_line | append",   required: &["path","action"] },
        Op { op: "patch", inner: "apply_patch", sig: "{path, patch}",    note: "unified diff (@@-hunk format)",      required: &["path","patch"] },
        Op { op: "diff",  inner: "file_diff",   sig: "{path}",           note: "vs first read/edit this conversation", required: &["path"] },
        Op { op: "undo",  inner: "file_undo",   sig: "{path}",           note: "restore to first snapshot",           required: &["path"] },
    ],
    extra_props: r#"
        "path": {"type":"string"},
        "content": {"type":"string"},
        "action": {"type":"string","enum":["search_replace","insert_at_line","append"]},
        "search": {"type":"string"},
        "replace": {"type":"string"},
        "line": {"type":"integer"},
        "patch": {"type":"string"}
    "#,
};

const SEMANTIC: Tool = Tool {
    name: "semantic",
    hint: "structural code edits via LSP (rename, code actions, symbol search). PREFER over fs_write for rename/extract/inline/organize-imports: the server updates references and imports safely. Needs rust-analyzer | typescript-language-server | pylsp on PATH.",
    ops: &[
        Op { op: "rename",           inner: "semantic_rename",           sig: "{path, line, character, new_name}",
             note: "0-based position; applies edit across the whole project", required: &["path","line","character","new_name"] },
        Op { op: "code_actions",     inner: "semantic_code_actions",     sig: "{path, line, character}",
             note: "lists available refactors/fixes at cursor with ids; apply one via op=apply_action", required: &["path","line","character"] },
        Op { op: "apply_action",     inner: "semantic_apply_action",     sig: "{id}",
             note: "id from a recent code_actions response, e.g. 'a0'", required: &["id"] },
        Op { op: "workspace_symbol", inner: "semantic_workspace_symbol", sig: "{query, hint_path?}",
             note: "fuzzy symbol search; hint_path picks the language server (default: scan cwd)", required: &["query"] },
    ],
    extra_props: r#"
        "path": {"type":"string"},
        "line": {"type":"integer"},
        "character": {"type":"integer"},
        "new_name": {"type":"string"},
        "id": {"type":"string"},
        "query": {"type":"string"},
        "hint_path": {"type":"string"}
    "#,
};

const WEB: Tool = Tool {
    name: "web",
    hint: "internet access. search for discovery, fetch for a known URL. for JS-rendered pages use browser.navigate instead.",
    ops: &[
        Op { op: "search", inner: "web_search", sig: "{query}", note: "DuckDuckGo; returns title/url/snippet", required: &["query"] },
        Op { op: "fetch",  inner: "fetch_url",  sig: "{url}",   note: "extract text from a URL",               required: &["url"] },
    ],
    extra_props: r#"
        "query": {"type":"string"},
        "url": {"type":"string"}
    "#,
};

const ALL: &[&Tool] = &[
    &MEMORY, &SCHEDULE, &GIT, &UI, &BROWSER, &SHELL, &LSP, &SEMANTIC, &FS_READ, &FS_WRITE, &WEB,
];

fn render_description(t: &Tool) -> String {
    let mut out = String::with_capacity(320);
    out.push_str(t.hint);
    out.push_str(" ops: ");
    for (i, op) in t.ops.iter().enumerate() {
        if i > 0 {
            out.push_str(" | ");
        }
        out.push_str(op.op);
        out.push_str(op.sig);
        if !op.note.is_empty() {
            out.push_str(" (");
            out.push_str(op.note);
            out.push(')');
        }
    }
    out
}

fn render_schema(t: &Tool) -> Value {
    let ops_enum: Vec<&str> = t.ops.iter().map(|o| o.op).collect();
    let schema_str = format!(
        r#"{{
            "type":"object",
            "properties": {{
                "op": {{"type":"string","enum":{enum}}},
                {extras}
            }},
            "required":["op"]
        }}"#,
        enum = serde_json::to_string(&ops_enum).unwrap(),
        extras = t.extra_props.trim(),
    );
    serde_json::from_str(&schema_str).unwrap_or_else(|e| {
        eprintln!("tool_dispatch: schema build failed for {}: {}", t.name, e);
        json!({"type":"object","properties":{"op":{"type":"string"}},"required":["op"]})
    })
}

/// Build the full tool list advertised to the LLM.
pub fn build_tools() -> Vec<ToolDefinition> {
    let mut out: Vec<ToolDefinition> = ALL
        .iter()
        .map(|t| ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: t.name.to_string(),
                description: render_description(t),
                parameters: render_schema(t),
            },
        })
        .collect();

    out.push(ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "todo_write".to_string(),
            description: "track multi-step work (3+ steps only). Pass FULL list each call - replaces prior. Keep exactly ONE in_progress; mark completed IMMEDIATELY when done.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content":    {"type":"string","description":"imperative, e.g. 'Run tests'"},
                                "activeForm": {"type":"string","description":"present continuous, e.g. 'Running tests'"},
                                "status":     {"type":"string","enum":["pending","in_progress","completed"]}
                            },
                            "required": ["content","activeForm","status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        },
    });

    out
}

// treat "" / null / [] / {} as missing. bools and numbers present if not null.
fn is_present(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(_) => true,
    }
}

fn usage_error(t: &Tool, problem: &str) -> String {
    let mut s = String::with_capacity(320);
    s.push_str(t.name);
    s.push_str(": ");
    s.push_str(problem);
    s.push_str(".\nvalid ops:\n");
    for op in t.ops {
        s.push_str("  ");
        s.push_str(op.op);
        s.push_str(op.sig);
        if !op.note.is_empty() {
            s.push_str("  - ");
            s.push_str(op.note);
        }
        s.push('\n');
    }
    s.push_str("example: {\"op\":\"");
    s.push_str(t.ops[0].op);
    s.push_str("\"");
    for r in t.ops[0].required {
        s.push_str(", \"");
        s.push_str(r);
        s.push_str("\":...");
    }
    s.push('}');
    s
}

fn missing_params_error(t: &Tool, op: &Op, missing: &[&str]) -> String {
    let list = missing.join(", ");
    let example_pairs: Vec<String> = op
        .required
        .iter()
        .map(|k| format!("\"{}\":...", k))
        .collect();
    format!(
        "{}: op='{}' missing required: {}.\nexpected {}{}\nexample: {{\"op\":\"{}\", {}}}",
        t.name,
        op.op,
        list,
        op.op,
        op.sig,
        op.op,
        example_pairs.join(", "),
    )
}

/// Resolve a possibly-consolidated tool call to the leaf handler name.
///
/// - `Ok(None)` = not consolidated, pass through.
/// - `Ok(Some(leaf))` = rewrite call to `leaf`. Args stay; leaf handlers ignore `op`.
/// - `Err(usage)` = malformed call; return directly to LLM.
pub fn dispatch(tool_name: &str, args: &Value) -> Result<Option<String>, String> {
    let Some(spec) = ALL.iter().find(|t| t.name == tool_name) else {
        return Ok(None);
    };
    let Some(op_name) = args.get("op").and_then(|v| v.as_str()) else {
        return Err(usage_error(spec, "missing required field 'op'"));
    };
    let Some(op) = spec.ops.iter().find(|o| o.op == op_name) else {
        return Err(usage_error(spec, &format!("unknown op '{}'", op_name)));
    };
    let missing: Vec<&str> = op
        .required
        .iter()
        .copied()
        .filter(|k| !is_present(args.get(*k)))
        .collect();
    if !missing.is_empty() {
        return Err(missing_params_error(spec, op, &missing));
    }
    Ok(Some(op.inner.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_unknown_tool() {
        let r = dispatch("not_a_tool", &json!({})).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn missing_op_gives_usage() {
        let err = dispatch("git", &json!({})).unwrap_err();
        assert!(err.contains("missing required field 'op'"));
        assert!(err.contains("status{}"));
        assert!(err.contains("commit{message, files}"));
    }

    #[test]
    fn unknown_op_gives_usage() {
        let err = dispatch("git", &json!({"op": "yolo"})).unwrap_err();
        assert!(err.contains("unknown op 'yolo'"));
    }

    #[test]
    fn rewrites_to_leaf_name() {
        let leaf = dispatch("git", &json!({"op": "status"})).unwrap();
        assert_eq!(leaf.as_deref(), Some("git_status"));
    }

    #[test]
    fn missing_required_params_listed() {
        let err = dispatch("git", &json!({"op":"commit"})).unwrap_err();
        assert!(err.contains("missing required: message, files"), "{}", err);
    }

    #[test]
    fn empty_string_counts_as_missing() {
        let err = dispatch("web", &json!({"op":"search","query":"  "})).unwrap_err();
        assert!(err.contains("missing required: query"), "{}", err);
    }

    #[test]
    fn empty_array_counts_as_missing() {
        let err = dispatch("git", &json!({"op":"commit","message":"x","files":[]})).unwrap_err();
        assert!(err.contains("missing required: files"), "{}", err);
    }

    #[test]
    fn all_present_dispatches() {
        let leaf = dispatch("web", &json!({"op":"fetch","url":"https://example.com"})).unwrap();
        assert_eq!(leaf.as_deref(), Some("fetch_url"));
    }

    #[test]
    fn numbers_satisfy_required() {
        let leaf = dispatch(
            "lsp",
            &json!({"op":"hover","path":"a.rs","line":0,"character":0}),
        )
        .unwrap();
        assert_eq!(leaf.as_deref(), Some("lsp_hover"));
    }

    #[test]
    fn semantic_rename_dispatches() {
        let leaf = dispatch(
            "semantic",
            &json!({"op":"rename","path":"x.rs","line":0,"character":0,"new_name":"foo"}),
        )
        .unwrap();
        assert_eq!(leaf.as_deref(), Some("semantic_rename"));
    }

    #[test]
    fn semantic_rename_missing_new_name() {
        let err = dispatch(
            "semantic",
            &json!({"op":"rename","path":"x.rs","line":0,"character":0}),
        )
        .unwrap_err();
        assert!(err.contains("missing required: new_name"), "{}", err);
    }

    #[test]
    fn semantic_workspace_symbol_optional_hint() {
        // hint_path is optional; query alone satisfies required
        let leaf = dispatch("semantic", &json!({"op":"workspace_symbol","query":"User"})).unwrap();
        assert_eq!(leaf.as_deref(), Some("semantic_workspace_symbol"));
    }

    #[test]
    fn every_tool_builds_valid_schema() {
        let tools = build_tools();
        assert_eq!(tools.len(), ALL.len() + 1); // +1 for todo_write
        for t in &tools {
            let params = &t.function.parameters;
            assert!(
                params.get("type").and_then(|v| v.as_str()) == Some("object"),
                "{}",
                t.function.name
            );
            assert!(params.get("properties").is_some(), "{}", t.function.name);
        }
    }
}
