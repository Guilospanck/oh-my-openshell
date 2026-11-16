//! The MCP (Model Context Protocol) server's protocol half: newline-delimited JSON-RPC 2.0
//! over the ops layer. `handle` does no I/O (one line in, at most one line out), so the
//! binary's stdin/stdout loop stays trivial. Request bodies are never logged: tool arguments
//! may carry anything.

use crate::ops::{self, Ctx};
use serde_json::{json, Map, Value};

const DEFAULT_PROTOCOL: &str = "2025-06-18";

const SPEC_DOC: &str = "Sandbox spec. Keys: name (required), image, providers (array of names), \
cpu, memory, command (run once, then the sandbox is deleted; its output is not returned; \
leave it out for a sandbox to exec in), toolchains (array), repo {url, env_var} (env_var is \
the NAME of an environment variable holding the access token, never the token itself, and \
must be one the server operator allows), share {path, rw} (refused unless the server \
operator enabled it; path must resolve, through symlinks, inside one of the operator's share \
root folders; risky folders such as home, the disk or system folders are refused even there).";

fn tools() -> Value {
    json!([
        {
            "name": "sandbox_list",
            "description": "List the sandboxes on the gateway: name, phase, created.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "sandbox_create",
            "description": "Create a sandbox from a spec, or only show the plan with dry_run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "spec": {"type": "object", "description": SPEC_DOC},
                    "dry_run": {"type": "boolean", "description": "Only return the plan."}
                },
                "required": ["spec"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_delete",
            "description": "Delete a sandbox and the repo providers osh made for it.",
            "inputSchema": {
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_exec",
            "description": "Run a command in a sandbox; returns exit_code, stdout, stderr.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1}
                },
                "required": ["name", "argv"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_apply",
            "description": "Create the sandbox unless one of that name exists.",
            "inputSchema": {
                "type": "object",
                "properties": {"spec": {"type": "object", "description": SPEC_DOC}},
                "required": ["spec"],
                "additionalProperties": false
            }
        }
    ])
}

type Fault = (i64, String);

fn reply(id: &Value, body: Result<Value, Fault>) -> String {
    let v = match body {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    // serde_json escapes newlines inside strings, so the output is a single line.
    v.to_string()
}

fn invalid(msg: impl Into<String>) -> Fault {
    (-32602, msg.into())
}

/// The `arguments` object, with any key outside `allowed` refused.
fn args_of(p: &Value, allowed: &[&str]) -> Result<Map<String, Value>, Fault> {
    let args = match p.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(invalid("`arguments` must be an object")),
    };
    for k in args.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(invalid(format!("unknown argument `{k}`")));
        }
    }
    Ok(args)
}

fn req_str(a: &Map<String, Value>, k: &str) -> Result<String, Fault> {
    match a.get(k) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(invalid(format!("argument `{k}` must be a string"))),
        None => Err(invalid(format!("missing argument `{k}`"))),
    }
}

fn req_spec(a: &Map<String, Value>) -> Result<Value, Fault> {
    match a.get("spec") {
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(_) => Err(invalid("argument `spec` must be an object")),
        None => Err(invalid("missing argument `spec`")),
    }
}

fn argv_of(a: &Map<String, Value>) -> Result<Vec<String>, Fault> {
    let bad = || invalid("argument `argv` must be an array of strings");
    match a.get("argv") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|x| x.as_str().map(String::from).ok_or_else(bad))
            .collect(),
        Some(_) => Err(bad()),
        None => Err(invalid("missing argument `argv`")),
    }
}

fn call(p: &Value, ctx: &Ctx) -> Result<Value, Fault> {
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing tool `name`"))?;
    let result = match name {
        "sandbox_list" => {
            args_of(p, &[])?;
            ops::sandbox_list(ctx)
        }
        "sandbox_create" => {
            let a = args_of(p, &["spec", "dry_run"])?;
            let spec = req_spec(&a)?;
            let dry = match a.get("dry_run") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(invalid("argument `dry_run` must be a boolean")),
            };
            ops::sandbox_create(ctx, &spec, dry)
        }
        "sandbox_delete" => {
            let a = args_of(p, &["name"])?;
            ops::sandbox_delete(ctx, &req_str(&a, "name")?)
        }
        "sandbox_exec" => {
            let a = args_of(p, &["name", "argv"])?;
            let n = req_str(&a, "name")?;
            ops::sandbox_exec(ctx, &n, &argv_of(&a)?)
        }
        "sandbox_apply" => {
            let a = args_of(p, &["spec"])?;
            ops::apply(ctx, &req_spec(&a)?)
        }
        other => return Err(invalid(format!("unknown tool `{other}`"))),
    };
    let is_error = result.is_err();
    let (out, _) = ops::envelope(result);
    Ok(json!({"content": [{"type": "text", "text": out.to_string()}], "isError": is_error}))
}

/// One request line in, the response line out; `None` for a notification (no `id`).
pub fn handle(line: &str, ctx: &Ctx) -> Option<String> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(reply(&Value::Null, Err((-32700, "parse error".into())))),
    };
    let Some(obj) = req.as_object() else {
        let m = if req.is_array() {
            "batch requests are not supported"
        } else {
            "invalid request"
        };
        return Some(reply(&Value::Null, Err((-32600, m.into()))));
    };
    let id = obj.get("id");
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Some(reply(
            id.unwrap_or(&Value::Null),
            Err((-32600, "invalid request: no method".into())),
        ));
    };
    let id = id?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let body = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "osh", "version": ctx.version},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(&params, ctx),
        m => Err((-32601, format!("method not found: {m}"))),
    };
    Some(reply(id, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::tests::Fake;

    fn ctx() -> Ctx {
        Ctx {
            dir: std::path::PathBuf::from("/nonexistent"),
            gateway: None,
            allow_share: false,
            refuse_risky_share: true,
            repo_vars: Some(Vec::new()),
            share_roots: Some(Vec::new()),
            version: "9.8.7-given".into(),
        }
    }

    fn ask(line: &str, c: &Ctx) -> Value {
        let out = handle(line, c).expect("a response");
        assert!(!out.contains('\n'), "framing: {out}");
        serde_json::from_str(&out).unwrap()
    }

    fn call_line(name: &str, args: Value) -> String {
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
               "params":{"name":name,"arguments":args}})
        .to_string()
    }

    #[test]
    fn initialize_echoes_version() {
        let r = ask(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
            &ctx(),
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "osh");
        // The binary's version, as given in the context, not osh-core's own.
        assert_eq!(r["result"]["serverInfo"]["version"], "9.8.7-given");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let d = ask(
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#,
            &ctx(),
        );
        assert_eq!(d["id"], "a");
        assert_eq!(d["result"]["protocolVersion"], DEFAULT_PROTOCOL);
    }

    #[test]
    fn tools_list_names_and_closed_schemas() {
        let r = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &ctx());
        let t = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = t.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "sandbox_list",
                "sandbox_create",
                "sandbox_delete",
                "sandbox_exec",
                "sandbox_apply"
            ]
        );
        for x in t {
            assert_eq!(x["inputSchema"]["additionalProperties"], false, "{x}");
        }
        for i in [1, 4] {
            let doc = t[i]["inputSchema"]["properties"]["spec"]["description"]
                .as_str()
                .unwrap();
            assert!(doc.contains("refused unless the server operator enabled it"));
            assert!(doc.contains("env_var") && !doc.contains("token_env"));
        }
    }

    #[test]
    fn ping_and_protocol_faults() {
        let c = ctx();
        let p = ask(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#, &c);
        assert_eq!(p["result"], json!({}));
        let m = ask(r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#, &c);
        assert_eq!(m["error"]["code"], -32601);
        let g = ask("garbage{", &c);
        assert_eq!(g["error"]["code"], -32700);
        assert!(g["id"].is_null());
        assert_eq!(ask("[]", &c)["error"]["code"], -32600);
        assert_eq!(ask("5", &c)["error"]["code"], -32600);
    }

    #[test]
    fn notifications_get_no_response() {
        let c = ctx();
        let n = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle(n, &c).is_none());
        assert!(handle(r#"{"jsonrpc":"2.0","method":"nope"}"#, &c).is_none());
    }

    #[test]
    fn unknown_tool_and_bad_arguments_are_invalid_params() {
        let c = ctx();
        let e = |name: &str, args: Value| ask(&call_line(name, args), &c)["error"].clone();
        assert_eq!(e("nope", json!({}))["code"], -32602);
        let m = e("sandbox_delete", json!({}));
        assert_eq!(m["code"], -32602);
        assert!(m["message"].as_str().unwrap().contains("name"));
        let m = e("sandbox_exec", json!({"name":"w1","argv":["ls", 3]}));
        assert_eq!(m["code"], -32602);
        assert!(m["message"].as_str().unwrap().contains("argv"));
        assert_eq!(
            e("sandbox_exec", json!({"name":"w1","argv":"ls"}))["code"],
            -32602
        );
        assert_eq!(e("sandbox_list", json!({"x":1}))["code"], -32602);
        let m = e(
            "sandbox_create",
            json!({"spec":{"name":"a"},"dry_run":"yes"}),
        );
        assert!(m["message"].as_str().unwrap().contains("dry_run"));
        assert_eq!(e("sandbox_apply", json!({"spec":[]}))["code"], -32602);
    }

    fn text_json(r: &Value) -> Value {
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn list_returns_json_text() {
        let f = Fake::new("mcp-list");
        std::env::set_var(
            "OSH_FAKE_SANDBOXES",
            r#"{"sandboxes":[{"name":"w1","phase":"Ready","created":"t"}]}"#,
        );
        let r = ask(&call_line("sandbox_list", json!({})), &f.ctx());
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["content"][0]["type"], "text");
        assert!(text_json(&r).is_array());
    }

    #[test]
    fn failing_operation_is_is_error_not_rpc_error() {
        let f = Fake::new("mcp-fail");
        let r = ask(
            &call_line("sandbox_delete", json!({"name":"../x"})),
            &f.ctx(),
        );
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(text_json(&r)["ok"], false);
    }

    #[test]
    fn exec_multiline_output_stays_one_line() {
        let f = Fake::new("mcp-exec");
        std::env::set_var("OSH_FAKE_EXEC_STDOUT", "a\nb");
        let line = call_line("sandbox_exec", json!({"name":"w1","argv":["echo","x"]}));
        let r = ask(&line, &f.ctx());
        assert_eq!(r["result"]["isError"], false);
        assert!(text_json(&r)["stdout"]
            .as_str()
            .unwrap()
            .starts_with("a\nb"));
    }

    #[test]
    fn share_refused_unless_operator_allows() {
        let f = Fake::new("mcp-share");
        let spec = json!({"name":"w1","share":{"path": f.dir.to_string_lossy(), "rw": false}});
        let line = call_line("sandbox_create", json!({"spec": spec, "dry_run": true}));
        let r = ask(&line, &f.ctx());
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(text_json(&r)["error"].as_str().unwrap().contains("share"));
        let mut c = f.ctx();
        c.allow_share = true;
        let r = ask(&line, &c);
        assert_eq!(r["result"]["isError"], false, "{r}");
        let r = ask(&call_line("sandbox_apply", json!({"spec": spec})), &f.ctx());
        assert_eq!(r["result"]["isError"], true, "{r}");
    }

    #[test]
    fn allow_share_cannot_be_smuggled_in() {
        let f = Fake::new("mcp-smuggle");
        let spec = json!({"name":"w1","share":{"path": f.dir.to_string_lossy(), "rw": false}});
        for tool in ["sandbox_create", "sandbox_apply"] {
            let r = ask(
                &call_line(tool, json!({"spec": spec, "allow_share": true})),
                &f.ctx(),
            );
            assert_eq!(r["error"]["code"], -32602, "{r}");
            assert!(r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("allow_share"));
            let mut inside = spec.clone();
            inside["allow_share"] = json!(true);
            let r = ask(&call_line(tool, json!({"spec": inside})), &f.ctx());
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(text_json(&r)["error"]
                .as_str()
                .unwrap()
                .contains("allow_share"));
        }
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn over_mcp_a_risky_share_and_an_unlisted_repo_var_are_refused() {
        let f = Fake::new("mcp-guards");
        // The disk as a root still meets the risky-folder check behind the roots.
        let c = Ctx {
            allow_share: true,
            share_roots: Some(vec!["/".into()]),
            ..ctx()
        };
        let spec = json!({"name":"w1","share":{"path":"/","rw":true}});
        let r = ask(
            &call_line("sandbox_create", json!({"spec": spec, "dry_run": true})),
            &c,
        );
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert!(text_json(&r)["error"].as_str().unwrap().contains("refused"));
        let spec = json!({"name":"w1","repo":{"url":"https://github.com/o/r","env_var":"PATH"}});
        let r = ask(&call_line("sandbox_apply", json!({"spec": spec})), &c);
        assert_eq!(r["result"]["isError"], true, "{r}");
        let e = text_json(&r)["error"].as_str().unwrap().to_string();
        assert!(e.contains("OSH_MCP_REPO_VARS"), "{e}");
        assert_eq!(f.calls(), "");
    }
}
