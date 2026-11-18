//! The MCP (Model Context Protocol) server's protocol half: newline-delimited JSON-RPC 2.0
//! over the ops layer. `handle` does no I/O (one line in, at most one line out), so the
//! binary's stdin/stdout loop stays trivial. Request bodies are never logged: tool arguments
//! may carry anything.

use crate::gateways;
use crate::jobs;
use crate::ops::{self, Ctx};
use serde_json::{json, Map, Value};

const DEFAULT_PROTOCOL: &str = "2025-06-18";

/// Standing rules every client is given when it connects. The server cannot stop a client's own
/// host tools; the user's Claude Code permissions do that. This is what an agent is told.
const INSTRUCTIONS: &str = "osh gives you sandboxes: do the user's work INSIDE them.\n\
\n\
Where work happens. When the user asks for work in a sandbox, do every command, file change, git \
commit and push through sandbox_exec (or a sandbox_create command); never run it on the user's own \
machine with your host tools (shell, editor, git), not even as a fallback. When something fails in \
the sandbox (a git push, a missing tool), do not redo it on the host: read the error, fix it in \
the sandbox or stop and tell the user.\n\
\n\
Before you create a sandbox, call sandbox_options. It lists the images, the providers and the \
toolchain groups there are. Toolchain groups and providers only OPEN NETWORK HOSTS: they install \
nothing. The tools a sandbox has come from its image (each image's `about` says what it holds). \
Before you create a sandbox, sort what the work needs: call image_guide. It says what a sandbox \
can install at run time (no root: user-level pip, npm, go and prebuilt binaries from open hosts) \
and what needs a new image (apt packages, compilers and linkers, Rust, builds of native code, \
browsers, databases), and how to write the Dockerfile. Tell the user which side each need falls \
on and why, then let them choose: a new image (you write the Dockerfile following the guide, \
paste the complete Dockerfile in a code block (not a summary) from image_build with dry_run true, and build it only after they agree, then create the \
sandbox from it). Before you write a Dockerfile, find out what each tool needs to run (search its official install docs): PATH, environment variables, config and cache folders, linkers and libraries. A sandbox does NOT apply the Dockerfile's ENV, so the image must work without it, and give image_build verify commands that prove it. Get all of this right in the first Dockerfile: the user must not find out afterwards that a new image is needed, or an install at run time within those limits. Never pick for them, and never \
pipe curl into a shell. \
Translate what the user wants into the spec: a language or tool \
(rust, go, node, python, zig...) means that toolchain group in spec.toolchains plus the provider \
of that name in spec.providers when sandbox_options lists one (one that needs a credential is refused with the user's steps: read them out, and never ask for the credential in the chat); a \
specific environment means an image from the list. Without an image a kept sandbox gets \
claude-code:latest; a throwaway one (spec.command) gets OpenShell's default. Name every \
toolchain the work needs: giving spec.toolchains selects exactly those groups. Use sandbox_create \
with dry_run first when unsure what a spec will do.\n\
\n\
Folders and repos. A folder the user wants the sandbox to read or write goes in spec.share \
(rw true to write); it appears at /sandbox/<folder name>, and you work there. A git repo goes in \
spec.repo; a share alone gives no git access. After the sandbox is up, check that each thing \
works with sandbox_exec before you call it ready: `ls /sandbox/<folder>`, `git ls-remote origin` \
(or the repo URL) for git, the compiler's `--version` for a toolchain. A repo that is not a share \
is cloned by you inside the sandbox. If a share or token is refused, the error is the exact steps \
to read to the user; never ask the user to paste a token into the chat, and never edit osh.toml \
for them.\n\
\n\
Long work. A command that may take over half a minute: sandbox_exec with wait_secs 0, then \
job_status and job_output. Delete an image (image_delete) only when the user asks for it. Delete a sandbox only when it was a throwaway one (spec.command, which deletes \
itself) or the user asks you to; never delete a sandbox that holds work, and never after long work. \
When you do remove access, tell the user that removing a stored token does not revoke it on the \
forge.";

const SPEC_DOC: &str = "Sandbox spec; call sandbox_options for the names to use. Keys: name (required), \
image (an image tag from sandbox_options; left out, a kept sandbox gets claude-code:latest and a \
throwaway one OpenShell's default), providers (array of names from sandbox_options: osh creates \
a listed profile that needs no credential), \
cpu, memory, command (run once, then the sandbox is deleted; its output is not returned; \
leave it out for a sandbox to exec in), toolchains (array of toolchain group names whose hosts \
the sandbox may read; left out, the groups that are open by default), repo {url, env_var} (env_var is \
the NAME of an environment variable holding the access token, never the token itself, and \
must be one the server operator allows or stored with `osh mcp secret set NAME`; pick a name for \
this repo, such as ELYRIA_GITHUB_TOKEN, not a bare GITHUB_TOKEN, or use the name the user gives; when a call is \
refused for that, tell the user to run that command themselves in a terminal outside this chat, \
and never ask them to paste a token to you; for a sandbox that already exists use \
sandbox_repo_grant instead, which needs no recreation), share {path, rw} (mounted at /sandbox/<folder name>; refused unless the server \
operator enabled it; path must resolve, through symlinks, inside one of the operator's share \
root folders; risky folders such as home, the disk or system folders are refused even there; when a share is \
refused for the settings, the error lists the commands the user runs themselves; never edit osh.toml for them).";

/// The tools with the optional `gateway` argument every one of them takes.
fn tools() -> Value {
    let mut t = base_tools();
    if let Some(list) = t.as_array_mut() {
        for tool in list {
            tool["inputSchema"]["properties"]["gateway"] = json!({
                "type": "string",
                "description": "Which gateway to act on: one osh knows (its main or a gateway osh added). Leave it out for the server's default."
            });
        }
    }
    t
}

fn base_tools() -> Value {
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
            "description": "Run a command in a sandbox. When it finishes within wait_secs (default 30) the reply is exit_code, stdout, stderr. Otherwise the reply is {job, status: \"running\"} and the command keeps running: follow it with job_status and job_output. wait_secs 0 returns a job at once (use it for long work). max_secs ends the job after that many seconds; the operator's own limit (mcp.job_max_secs) caps it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "wait_secs": {"type": "integer", "minimum": 0, "maximum": 900, "description": "How long to wait for the command before returning a job (default 30)."},
                    "max_secs": {"type": "integer", "minimum": 0, "description": "End the job after this many seconds."}
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
        },
        {
            "name": "job_status",
            "description": "State of a job started by sandbox_exec: running, exited (with exit_code), cancelled or lost (its process or sandbox is gone). Job calls use the gateway the job was started on; a gateway argument is ignored.",
            "inputSchema": {
                "type": "object",
                "properties": {"job": {"type": "string"}},
                "required": ["job"],
                "additionalProperties": false
            }
        },
        {
            "name": "job_output",
            "description": "New output of a job from the given offsets: stdout, stderr, the offsets to pass next time, and more (true when output remains beyond them). Offsets count bytes and start at 0; a reply holds at most max_bytes (default and limit 65536) of each stream. Uses the gateway the job was started on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job": {"type": "string"},
                    "stdout_offset": {"type": "integer", "minimum": 0},
                    "stderr_offset": {"type": "integer", "minimum": 0},
                    "max_bytes": {"type": "integer", "minimum": 16, "maximum": 65536}
                },
                "required": ["job"],
                "additionalProperties": false
            }
        },
        {
            "name": "job_cancel",
            "description": "End a running job and every process it started. A job that already exited stays exited. Uses the gateway the job was started on.",
            "inputSchema": {
                "type": "object",
                "properties": {"job": {"type": "string"}},
                "required": ["job"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_repo_grant",
            "description": "Give a RUNNING sandbox access to one git repo (no recreation or restart): attaches the repo provider and configures git in the sandbox. env_var is the NAME of the token (never the token). It needs a token the user stored with `osh mcp secret set NAME` in a terminal of their own; when there is none, the error is the exact set of steps to read to the user (create the token at the given page, run that command, tell you), and you must not ask for the token in the chat. Tell the user that `osh mcp secret remove` only deletes osh's stored copy: the gateway's provider keeps its own copy and the token stays valid on the forge until sandbox_repo_revoke (or deleting the sandbox) and deleting the token there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The sandbox."},
                    "repo": {"type": "object", "properties": {"url": {"type": "string"}, "env_var": {"type": "string"}}, "required": ["url", "env_var"], "additionalProperties": false}
                },
                "required": ["name", "repo"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_repo_revoke",
            "description": "Take a repo grant away from a sandbox: detach and delete its repo provider. This does not make the token invalid: tell the user to delete it on the forge's token page (github.com/settings/personal-access-tokens), then run `osh mcp secret remove NAME` to forget osh's stored copy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "repo": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"], "additionalProperties": false}
                },
                "required": ["name", "repo"],
                "additionalProperties": false
            }
        },
        {
            "name": "sandbox_options",
            "description": "What a sandbox spec may name: images (and the default for a kept sandbox), providers (existing, and profiles osh can create), toolchain groups. Read-only; call it before sandbox_create.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "image_guide",
            "description": "How to decide whether work needs a new sandbox image and how to write its Dockerfile: what a sandbox can and cannot install at run time, what needs root or a system package, the Dockerfile rules, and the Dockerfiles osh ships as examples. Read-only; call it before sandbox_create when the work needs tools, and before image_build.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        },
        {
            "name": "image_build",
            "description": "Build a new sandbox image from a Dockerfile you wrote (follow image_guide). Always call it first with dry_run true and paste the COMPLETE Dockerfile and the verify list to the user in a code block (a summary is not enough; tool output is collapsed in their view); build (minutes, downloads a lot) only after they agree. The build context is empty (nothing can be copied in), osh's own base images are built first if missing, an existing image is never overwritten. Then name the image as spec.image.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Lowercase letters, digits and `-`: the image becomes NAME:latest."},
                    "dockerfile": {"type": "string", "description": "The whole Dockerfile text."},
                    "verify": {"type": "array", "items": {"type": "string"}, "description": "Shell commands the work needs this image to run (`cargo --version`, `cc --version`, a compile of a hello world). Required for a build. osh runs each in the new image with a clean environment (no Dockerfile ENV, like the sandbox) and removes the image if one fails."},
                    "dry_run": {"type": "boolean", "description": "Only check it and return it; write and build nothing."}
                },
                "required": ["name", "dockerfile"],
                "additionalProperties": false
            }
        },
        {
            "name": "image_delete",
            "description": "Delete one of osh's own images (the ones sandbox_options lists, as in the Images tab): the built image, and with remove_dockerfile true its Dockerfile too (not for a Dockerfile that ships with osh). Any other image is refused, and an image a sandbox still runs from is not forced. Destructive: only when the user asks for it or agrees, never on your own to tidy up; deleting claude-code:latest means the next kept sandbox rebuilds it (minutes).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The image, with or without :latest."},
                    "remove_dockerfile": {"type": "boolean", "description": "Also delete its Dockerfile from osh's images folder (default false)."}
                },
                "required": ["name"],
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
        if !allowed.contains(&k.as_str()) && k != "gateway" {
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

/// An optional whole number of zero or more.
fn opt_u64(a: &Map<String, Value>, k: &str) -> Result<Option<u64>, Fault> {
    match a.get(k) {
        None => Ok(None),
        Some(Value::Number(n)) if n.as_u64().is_some() => Ok(n.as_u64()),
        Some(_) => Err(invalid(format!(
            "argument `{k}` must be a whole number, zero or more"
        ))),
    }
}

fn job_json(j: &jobs::Job) -> Value {
    json!({
        "job": j.id, "state": j.state, "exit_code": j.exit_code,
        "sandbox": j.sandbox, "started": j.started,
    })
}

/// New output of both streams of a job from the given offsets.
fn job_output(ctx: &Ctx, id: &str, so: u64, eo: u64, limit: usize) -> Result<Value, String> {
    let out = jobs::output(ctx, id, jobs::Stream::Out, so, limit)?;
    let err = jobs::output(ctx, id, jobs::Stream::Err, eo, limit)?;
    Ok(json!({
        "stdout": out.text, "stderr": err.text,
        "stdout_offset": out.next, "stderr_offset": err.next,
        "more": out.more || err.more,
    }))
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
    let named = match p.get("arguments").and_then(|a| a.get("gateway")) {
        None | Some(Value::Null) => None,
        Some(Value::String(g)) => Some(g.as_str()),
        Some(_) => return Err(invalid("argument `gateway` must be a string")),
    };
    let gateway = gateways::resolve(ctx, named).map_err(invalid)?;
    let scoped = Ctx {
        gateway,
        ..ctx.clone()
    };
    let ctx = &scoped;
    let result = match name {
        "sandbox_list" => {
            args_of(p, &[])?;
            ops::sandbox_list(ctx)
        }
        "sandbox_options" => {
            args_of(p, &[])?;
            ops::sandbox_options(ctx)
        }
        "image_guide" => {
            args_of(p, &[])?;
            ops::image_guide(ctx)
        }
        "image_build" => {
            let a = args_of(p, &["name", "dockerfile", "verify", "dry_run"])?;
            let verify: Vec<String> = match a.get("verify") {
                None => Vec::new(),
                Some(Value::Array(v)) => v
                    .iter()
                    .map(|x| x.as_str().map(str::to_string))
                    .collect::<Option<_>>()
                    .ok_or_else(|| invalid("argument `verify` must be a list of strings"))?,
                Some(_) => return Err(invalid("argument `verify` must be a list of strings")),
            };
            let dry = match a.get("dry_run") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(invalid("argument `dry_run` must be a boolean")),
            };
            ops::image_build(
                ctx,
                &req_str(&a, "name")?,
                &req_str(&a, "dockerfile")?,
                &verify,
                dry,
            )
        }
        "image_delete" => {
            let a = args_of(p, &["name", "remove_dockerfile"])?;
            let rm = match a.get("remove_dockerfile") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(invalid("argument `remove_dockerfile` must be a boolean")),
            };
            ops::image_delete(ctx, &req_str(&a, "name")?, rm)
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
            let a = args_of(p, &["name", "argv", "wait_secs", "max_secs"])?;
            let n = req_str(&a, "name")?;
            let argv = argv_of(&a)?;
            let wait =
                opt_u64(&a, "wait_secs")?.map_or(jobs::DEFAULT_WAIT, |w| w.min(jobs::MAX_WAIT));
            let max = opt_u64(&a, "max_secs")?;
            jobs::exec(ctx, &n, &argv, wait, max)
        }
        "job_status" => {
            let a = args_of(p, &["job"])?;
            jobs::status(ctx, &req_str(&a, "job")?).map(|j| job_json(&j))
        }
        "job_output" => {
            let a = args_of(p, &["job", "stdout_offset", "stderr_offset", "max_bytes"])?;
            let id = req_str(&a, "job")?;
            let offset = |k: &str| -> Result<u64, Fault> {
                match opt_u64(&a, k)? {
                    Some(v) if v > jobs::MAX_OFFSET => Err(invalid(format!(
                        "argument `{k}` must be at most {}",
                        jobs::MAX_OFFSET
                    ))),
                    v => Ok(v.unwrap_or(0)),
                }
            };
            let so = offset("stdout_offset")?;
            let eo = offset("stderr_offset")?;
            let limit = opt_u64(&a, "max_bytes")?
                .map_or(jobs::CHUNK as u64, |m| m.min(jobs::CHUNK as u64))
                as usize;
            job_output(ctx, &id, so, eo, limit)
        }
        "job_cancel" => {
            let a = args_of(p, &["job"])?;
            jobs::cancel(ctx, &req_str(&a, "job")?).map(|j| job_json(&j))
        }
        "sandbox_apply" => {
            let a = args_of(p, &["spec"])?;
            ops::apply(ctx, &req_spec(&a)?)
        }
        "sandbox_repo_grant" | "sandbox_repo_revoke" => {
            let a = args_of(p, &["name", "repo"])?;
            let n = req_str(&a, "name")?;
            let Some(Value::Object(r)) = a.get("repo") else {
                return Err(invalid("argument `repo` must be an object"));
            };
            let field = |k: &str| match r.get(k) {
                Some(Value::String(s)) => Ok(s.clone()),
                _ => Err(invalid(format!("`repo.{k}` must be a string"))),
            };
            let grant = name == "sandbox_repo_grant";
            for k in r.keys() {
                if k != "url" && !(grant && k == "env_var") {
                    return Err(invalid(format!("unknown field `repo.{k}`")));
                }
            }
            if grant {
                ops::repo_grant(ctx, &n, &field("url")?, &field("env_var")?)
            } else {
                ops::repo_revoke(ctx, &n, &field("url")?)
            }
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
            "instructions": INSTRUCTIONS,
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
            job_max_secs: 0,
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
        // Standing rules for the agent: work inside the sandbox, never fall back to the host.
        let rules = r["result"]["instructions"].as_str().unwrap();
        assert!(
            rules.contains("INSIDE") && rules.contains("never run it on the user's own machine")
        );
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
                "sandbox_apply",
                "job_status",
                "job_output",
                "job_cancel",
                "sandbox_repo_grant",
                "sandbox_repo_revoke",
                "sandbox_options",
                "image_guide",
                "image_build",
                "image_delete"
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
    fn every_tool_accepts_an_optional_gateway() {
        let r = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &ctx());
        for x in r["result"]["tools"].as_array().unwrap() {
            assert_eq!(
                x["inputSchema"]["properties"]["gateway"]["type"], "string",
                "{x}"
            );
            assert_eq!(x["inputSchema"]["additionalProperties"], false, "{x}");
            assert!(!x["inputSchema"]["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|n| n == "gateway")));
        }
    }

    #[test]
    fn an_unknown_gateway_is_invalid_params_and_runs_nothing() {
        let f = Fake::new("mcp-gw-unknown");
        for bad in ["nope", "", "-x", "a/b"] {
            let r = ask(
                &call_line("sandbox_list", json!({"gateway": bad})),
                &f.ctx(),
            );
            assert_eq!(r["error"]["code"], -32602, "{bad:?}: {r}");
        }
        assert_eq!(std::fs::read_to_string(&f.log).unwrap(), "");
    }

    #[test]
    fn a_known_gateway_is_used_for_that_call_only() {
        let f = Fake::new("mcp-gw-known");
        std::fs::write(
            f.dir.join("gateways.json"),
            r#"{"gateways":[{"name":"openshell-docker","kind":"docker","port":17671,"main":false}]}"#,
        )
        .unwrap();
        std::env::set_var("OSH_FAKE_SANDBOXES", r#"{"sandboxes":[]}"#);
        let r = ask(
            &call_line("sandbox_list", json!({"gateway": "openshell-docker"})),
            &f.ctx(),
        );
        assert_eq!(r["result"]["isError"], false, "{r}");
        let log = std::fs::read_to_string(&f.log).unwrap();
        assert!(log.contains("-g openshell-docker"), "{log}");
        std::fs::write(&f.log, "").unwrap();
        ask(&call_line("sandbox_list", json!({})), &f.ctx());
        assert!(!std::fs::read_to_string(&f.log).unwrap().contains("-g"));
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

    /// A fake `openshell` whose `sandbox exec` really runs the command, for tools that start jobs.
    fn running(tag: &str) -> Fake {
        let f = Fake::new(tag);
        std::env::set_var("OSH_FAKE_EXEC_RUN", "1");
        f
    }

    /// Remove the job folders a test left in the "sandbox" (a folder on this machine).
    fn forget_jobs(f: &Fake) {
        for j in crate::jobs::list(&f.dir) {
            let _ = std::fs::remove_dir_all(format!("{}/{}", crate::jobs::sandbox_dir(), j.id));
        }
    }

    fn job_id(r: &Value) -> String {
        text_json(r)["job"].as_str().expect("a job id").to_string()
    }

    #[test]
    fn tools_list_has_the_job_tools_with_closed_schemas() {
        let r = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &ctx());
        let t = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = t.iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(&names[5..8], ["job_status", "job_output", "job_cancel"]);
        for x in &t[5..8] {
            assert_eq!(x["inputSchema"]["additionalProperties"], false, "{x}");
            assert_eq!(x["inputSchema"]["required"], json!(["job"]), "{x}");
        }
        let props = &t[3]["inputSchema"]["properties"];
        assert_eq!(props["wait_secs"]["type"], "integer");
        assert_eq!(props["max_secs"]["type"], "integer");
        assert!(t[3]["description"].as_str().unwrap().contains("job_status"));
    }

    #[test]
    fn sandbox_exec_takes_wait_and_max_secs_and_refuses_bad_values() {
        let c = ctx();
        let code = |args: Value| ask(&call_line("sandbox_exec", args), &c)["error"]["code"].clone();
        for bad in [
            json!({"name":"w1","argv":["ls"],"wait_secs":-1}),
            json!({"name":"w1","argv":["ls"],"wait_secs":1.5}),
            json!({"name":"w1","argv":["ls"],"wait_secs":"5"}),
            json!({"name":"w1","argv":["ls"],"max_secs":-1}),
            json!({"name":"w1","argv":["ls"],"max_secs":1e30}),
            json!({"name":"w1","argv":["ls"],"max_secs":null}),
        ] {
            assert_eq!(code(bad.clone()), -32602, "{bad}");
        }
        // Too long a wait is clamped, not refused: the command still runs.
        let f = running("mcp-clamp");
        let r = ask(
            &call_line(
                "sandbox_exec",
                json!({"name":"w1","argv":["true"],"wait_secs":901}),
            ),
            &f.ctx(),
        );
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert_eq!(text_json(&r)["exit_code"], 0);
        forget_jobs(&f);
    }

    #[test]
    fn over_mcp_a_quick_command_returns_the_result_and_a_slow_one_a_job() {
        let f = running("mcp-slow");
        let c = f.ctx();
        let slow = ask(
            &call_line(
                "sandbox_exec",
                json!({"name":"w1","argv":["sh","-c","sleep 2; echo late; exit 5"],"wait_secs":0}),
            ),
            &c,
        );
        assert_eq!(text_json(&slow)["status"], "running", "{slow}");
        let id = job_id(&slow);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let state = loop {
            let s = ask(&call_line("job_status", json!({"job": id})), &c);
            assert_eq!(s["result"]["isError"], false, "{s}");
            let v = text_json(&s);
            if v["state"] != "running" || std::time::Instant::now() > end {
                break v;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        assert_eq!(
            (state["state"].as_str(), state["exit_code"].as_i64()),
            (Some("exited"), Some(5))
        );
        assert_eq!(state["sandbox"], "w1");
        let out = text_json(&ask(&call_line("job_output", json!({"job": id})), &c));
        assert_eq!(out["stdout"], "late\n");
        assert_eq!(
            (out["stdout_offset"].as_u64(), out["stderr_offset"].as_u64()),
            (Some(5), Some(0))
        );
        assert_eq!(out["more"], false);
        let again = text_json(&ask(
            &call_line("job_output", json!({"job": id, "stdout_offset": 5})),
            &c,
        ));
        assert_eq!(again["stdout"], "");
        forget_jobs(&f);
    }

    #[test]
    fn job_output_offsets_above_the_ceiling_are_invalid_params() {
        let c = ctx();
        for bad in [4_611_686_018_427_387_905u64, u64::MAX] {
            let r = ask(
                &call_line(
                    "job_output",
                    json!({"job":"j0123456789abcdef","stdout_offset":bad}),
                ),
                &c,
            );
            assert_eq!(r["error"]["code"], -32602, "{bad}: {r}");
        }
    }

    #[test]
    fn the_job_tools_say_the_jobs_own_gateway_is_used() {
        let r = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, &ctx());
        for t in &r["result"]["tools"].as_array().unwrap()[5..8] {
            let d = t["description"].as_str().unwrap();
            assert!(d.contains("started on"), "{d}");
        }
    }

    #[test]
    fn job_output_offsets_must_be_non_negative_integers() {
        let c = ctx();
        for bad in [
            json!({"job":"j0123456789abcdef","stdout_offset":-1}),
            json!({"job":"j0123456789abcdef","stderr_offset":1.5}),
            json!({"job":"j0123456789abcdef","stdout_offset":"3"}),
            json!({"job":"j0123456789abcdef","stdout_offset":1e30}),
            json!({"job":"j0123456789abcdef","max_bytes":-5}),
        ] {
            let r = ask(&call_line("job_output", bad.clone()), &c);
            assert_eq!(r["error"]["code"], -32602, "{bad}: {r}");
        }
    }

    #[test]
    fn job_tools_refuse_an_unknown_or_malformed_job_id_without_reaching_openshell() {
        let f = Fake::new("mcp-badjob");
        for tool in ["job_status", "job_output", "job_cancel"] {
            for id in ["../x", "j0000000000000000", ""] {
                let r = ask(&call_line(tool, json!({"job": id})), &f.ctx());
                assert!(r.get("error").is_none(), "{r}");
                assert_eq!(r["result"]["isError"], true, "{tool} {id:?}");
                assert!(text_json(&r)["error"]
                    .as_str()
                    .unwrap()
                    .contains("unknown job"));
            }
            let r = ask(&call_line(tool, json!({})), &f.ctx());
            assert_eq!(r["error"]["code"], -32602, "{tool} needs a job");
        }
        assert_eq!(f.calls(), "");
    }

    #[test]
    fn the_caller_cannot_raise_the_operators_cap() {
        let f = running("mcp-cap");
        let mut c = f.ctx();
        c.job_max_secs = 100;
        let ask_for = |max: Value| {
            let mut args = json!({"name":"w1","argv":["true"],"wait_secs":0});
            if !max.is_null() {
                args["max_secs"] = max;
            }
            job_id(&ask(&call_line("sandbox_exec", args), &c))
        };
        let greedy = ask_for(json!(500));
        let modest = ask_for(json!(50));
        let none = ask_for(Value::Null);
        let calls = f.calls();
        let dir = crate::jobs::sandbox_dir();
        assert!(calls.contains(&format!("{dir}/{greedy} 100 ")), "{calls}");
        assert!(calls.contains(&format!("{dir}/{modest} 50 ")), "{calls}");
        assert!(calls.contains(&format!("{dir}/{none} 100 ")), "{calls}");
        forget_jobs(&f);
    }

    #[test]
    fn a_job_call_uses_the_gateway_the_job_was_started_on() {
        let f = running("mcp-jobgw");
        std::fs::write(
            f.dir.join("gateways.json"),
            r#"{"gateways":[{"name":"openshell-docker","kind":"docker","port":17671,"main":false}]}"#,
        )
        .unwrap();
        let c = f.ctx();
        let started = ask(
            &call_line(
                "sandbox_exec",
                json!({"name":"w1","argv":["sleep","1"],"wait_secs":0,"gateway":"openshell-docker"}),
            ),
            &c,
        );
        let id = job_id(&started);
        std::fs::write(&f.log, "").unwrap();
        let s = ask(&call_line("job_status", json!({"job": id})), &c);
        assert_eq!(s["result"]["isError"], false, "{s}");
        let calls = f.calls();
        assert!(
            calls
                .lines()
                .next()
                .unwrap()
                .starts_with("-g openshell-docker sandbox exec"),
            "{calls}"
        );
        forget_jobs(&f);
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
        let f = running("mcp-exec");
        let line = call_line(
            "sandbox_exec",
            json!({"name":"w1","argv":["sh","-c","printf 'a\\nb'"],"wait_secs":10}),
        );
        let r = ask(&line, &f.ctx());
        assert_eq!(r["result"]["isError"], false, "{r}");
        assert_eq!(text_json(&r)["stdout"], "a\nb");
        forget_jobs(&f);
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
