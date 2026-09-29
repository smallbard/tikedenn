//! Serveur MCP (Model Context Protocol, transport « Streamable HTTP » en
//! mode sans état) sur `POST /mcp`, pour Claude Code et tout client MCP :
//! mêmes opérations que l'API `/api/v1` (voir `api::op_*`), même token.
//!
//!   claude mcp add --transport http tikedenn https://<site>/mcp \
//!       --header "Authorization: Bearer tkd_…"

use crate::api::{self, ApiError};
use crate::tokens::TokenScope;
use crate::workflow::Actor;
use crate::AppState;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tikedenn_common::api::{Claim, ClaimNext, CreateTask, MoveTask, NewComment, PatchTask, Release};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Tikedenn is the shared kanban board where humans queue work for AI agents \
(states: draft → todo → progress → review → deploy → done). Typical loop: claim_next_task (or claim_task) \
to take a 'todo' task, read its description, acceptance criteria and log (review feedback from a previous \
round is in the log), work on the task's git branch (set_branch if empty, e.g. task/<id>-<slug>), push the \
branch, then submit_for_review with a summary of what changed and how it was tested. Never merge into the \
base branch yourself: a human reviews and approves, then the deploy worker merges and publishes. If blocked, \
submit_for_review with a note starting with 'BLOCKED:' and your question.";

/// Outils exposés : (nom, description, schéma des arguments, écriture ?).
fn tools() -> Vec<(&'static str, &'static str, Value, bool)> {
    let id = json!({"type": "integer", "description": "Task id"});
    vec![
        ("list_projects", "List projects (optionally the one whose git repository matches `repo`).",
         json!({"type": "object", "properties": {"repo": {"type": "string", "description": "Git remote URL (https, ssh or scp form)"}}}), false),
        ("list_tasks", "List tasks, optionally filtered by status (draft, todo, progress, review, deploy, done) and project (id or name).",
         json!({"type": "object", "properties": {"status": {"type": "string"}, "project": {"type": "string"}}}), false),
        ("get_task", "Full task: description, acceptance criteria, branch, files, dependencies and log (comments, reports, review feedback).",
         json!({"type": "object", "properties": {"id": id}, "required": ["id"]}), false),
        ("claim_next_task", "Claim the next available 'todo' task (highest priority first, not blocked) and return it; null if none. The claim is a lease: renew it with claim_task during long work.",
         json!({"type": "object", "properties": {
             "projects": {"type": "array", "items": {"type": "string"}, "description": "Accepted projects (ids or names); empty = all"},
             "ttl_minutes": {"type": "integer", "description": "Lease duration (default 60)"}}}), true),
        ("claim_task", "Claim a specific task (todo → progress) or renew your lease on it.",
         json!({"type": "object", "properties": {"id": id, "ttl_minutes": {"type": "integer"}}, "required": ["id"]}), true),
        ("set_branch", "Set the git branch of a task (e.g. task/42-short-title).",
         json!({"type": "object", "properties": {"id": id, "branch": {"type": "string"}}, "required": ["id", "branch"]}), true),
        ("add_comment", "Add a note to the task log (progress note, question…).",
         json!({"type": "object", "properties": {"id": id, "body": {"type": "string"}}, "required": ["id", "body"]}), true),
        ("submit_for_review", "Move your task to review with a report: what changed, how it was tested, open questions. The branch must be pushed first.",
         json!({"type": "object", "properties": {"id": id, "report": {"type": "string"}}, "required": ["id", "report"]}), true),
        ("release_task", "Give a claimed task back (back to todo) with an explanation.",
         json!({"type": "object", "properties": {"id": id, "note": {"type": "string"}}, "required": ["id"]}), true),
        ("create_task", "Propose a follow-up task (created in draft; a human decides whether it becomes todo).",
         json!({"type": "object", "properties": {
             "title": {"type": "string"}, "description": {"type": "string"}, "acceptance": {"type": "string"},
             "project": {"type": "string", "description": "Project id or name"},
             "priority": {"type": "integer", "description": "0 low, 1 normal, 2 high, 3 urgent"}}, "required": ["title"]}), true),
    ]
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_id(args: &Value) -> Result<i64, ApiError> {
    args.get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Missing integer argument 'id'."))
}

fn parse<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, ApiError> {
    serde_json::from_value(args.clone())
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("Invalid arguments: {e}")))
}

fn to_value<T: serde::Serialize>(v: Result<T, ApiError>) -> Result<Value, ApiError> {
    v.and_then(|v| {
        serde_json::to_value(v).map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    })
}

/// Exécute un outil et renvoie son résultat JSON.
fn call_tool(state: &AppState, actor: &Actor, name: &str, args: &Value) -> Result<Value, ApiError> {
    let conn = state.conn()?;
    let conn = &*conn;
    match name {
        "list_projects" => to_value(api::op_projects(conn, actor, arg_str(args, "repo"))),
        "list_tasks" => to_value(api::op_list_tasks(conn, actor, arg_str(args, "status"), arg_str(args, "project"))),
        "get_task" => to_value(api::op_get_task(conn, actor, arg_id(args)?)),
        "claim_next_task" => to_value(api::op_claim_next(conn, actor, &parse::<ClaimNext>(args)?)),
        "claim_task" => to_value(api::op_claim(conn, actor, arg_id(args)?, &parse::<Claim>(args)?)),
        "set_branch" => to_value(api::op_patch_task(conn, actor, arg_id(args)?, &parse::<PatchTask>(args)?)),
        "add_comment" => {
            let c = NewComment { body: arg_str(args, "body").unwrap_or_default().into(), kind: None };
            api::op_comment(conn, actor, arg_id(args)?, &c).map(|_| json!({"ok": true}))
        }
        "submit_for_review" => {
            let m = MoveTask { status: "review".into(), note: arg_str(args, "report").map(String::from) };
            to_value(api::op_move(conn, actor, arg_id(args)?, &m))
        }
        "release_task" => {
            let r = Release { status: Some("todo".into()), note: arg_str(args, "note").map(String::from) };
            to_value(api::op_release(conn, actor, arg_id(args)?, &r))
        }
        "create_task" => to_value(api::op_create_task(conn, actor, &parse::<CreateTask>(args)?)),
        _ => Err(ApiError::new(StatusCode::NOT_FOUND, format!("Unknown tool '{name}'."))),
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Traite un message JSON-RPC ; `None` pour une notification (sans réponse).
fn handle(state: &AppState, actor: &Actor, scope: TokenScope, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = SUPPORTED_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&SUPPORTED_VERSIONS[0]);
            rpc_result(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tikedenn", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => {
            let list: Vec<Value> = tools()
                .into_iter()
                .filter(|(_, _, _, write)| !(scope.read_only && *write))
                .map(|(name, description, schema, _)| {
                    json!({"name": name, "description": description, "inputSchema": schema})
                })
                .collect();
            rpc_result(&id, json!({"tools": list}))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let write = tools().iter().find(|t| t.0 == name).map(|t| t.3);
            let outcome = match write {
                None => return Some(rpc_error(&id, -32602, &format!("Unknown tool '{name}'."))),
                Some(true) if scope.read_only => Err(ApiError::new(StatusCode::FORBIDDEN, "This token is read-only.")),
                Some(_) => call_tool(state, actor, name, &args),
            };
            let (text, is_error) = match outcome {
                Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                Err(e) => (e.message, true),
            };
            rpc_result(&id, json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        _ => rpc_error(&id, -32601, &format!("Method not found: {method}")),
    })
}

/// Point d'entrée `POST /mcp` : un message (ou un lot) JSON-RPC.
pub async fn post(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(scope): Extension<TokenScope>,
    Json(body): Json<Value>,
) -> Response {
    // Les opérations sont courtes (SQLite local) : exécution directe.
    let reply = match &body {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch.iter().filter_map(|m| handle(&state, &actor, scope, m)).collect();
            (!replies.is_empty()).then(|| Value::Array(replies))
        }
        msg => handle(&state, &actor, scope, msg),
    };
    match reply {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET /mcp` : pas de flux SSE serveur → client (serveur sans état).
pub async fn get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
