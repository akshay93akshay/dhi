//! MCP health/status and live management routes (live reload).
//!
//! The config file remains the source of truth: add, edit, and delete persist
//! through [`dhi_mcp::edit`] first, then ask the running [`McpManager`] to
//! reload from disk. Responses never echo secret header values; list and
//! write responses use masked summaries.
//!
//! [`McpManager`]: crate::mcp::McpManager

use std::path::PathBuf;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::mcp::ValidatedServer;
use crate::mcp::reload::{ReloadReport, ServerSummary, summarize};
use crate::{ApiError, AppState};

/// `GET /api/mcp/servers`: every configured server's health, including
/// servers that failed to connect. When servers exist, a `config` array
/// carries their masked configuration (never header values).
pub(crate) async fn list_servers(State(state): State<AppState>) -> Json<Value> {
    let servers = state.mcp.statuses().await;
    let config = state.mcp.server_summaries();
    let mut body = json!({ "servers": servers });
    if !config.is_empty() {
        body["config"] = serde_json::to_value(config).unwrap_or(Value::Null);
    }
    Json(body)
}

/// `GET /api/mcp/servers/{name}/tools`: the cached discovered tools of one
/// configured server. Unknown names are `404`; a configured but disconnected
/// server returns its (empty) cache.
pub(crate) async fn server_tools(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tools = state
        .mcp
        .tools(&name)
        .await
        .ok_or_else(|| ApiError::not_found(format!("MCP server {name:?}")))?;
    Ok(Json(json!({ "server": name, "tools": tools })))
}

/// `POST /api/mcp/servers`: add one server, persist it, and reload.
pub(crate) async fn add_server(
    State(state): State<AppState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = json_body(payload)?;
    let (name, entry) = parse_entry_body(body, None)?;
    let path = config_path(&state)?;
    dhi_mcp::edit::add_server(&path, &name, &entry).map_err(edit_error)?;
    let reload = reload_after_write(&state, Some(&name)).await?;
    let summary = summary_for(&name, &entry)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "server": summary, "reload": reload })),
    ))
}

/// `PUT`/`PATCH /api/mcp/servers/{name}`: replace one existing server,
/// persist it, and reconnect it.
pub(crate) async fn update_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = json_body(payload)?;
    let (name, entry) = parse_entry_body(body, Some(&name))?;
    let path = config_path(&state)?;
    dhi_mcp::edit::update_server(&path, &name, &entry).map_err(edit_error)?;
    let reload = reload_after_write(&state, Some(&name)).await?;
    let summary = summary_for(&name, &entry)?;
    Ok((
        StatusCode::OK,
        Json(json!({ "server": summary, "reload": reload })),
    ))
}

/// `DELETE /api/mcp/servers/{name}`: remove one server, persist, and reload.
pub(crate) async fn delete_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = config_path(&state)?;
    let name = name.trim().to_string();
    dhi_mcp::edit::delete_server(&path, &name).map_err(edit_error)?;
    let reload = reload_after_write(&state, None).await?;
    Ok(Json(json!({ "deleted": name, "reload": reload })))
}

/// `POST /api/mcp/reload`: re-read the config file and apply it live. A
/// connect failure is reported per server in `reload.failed`; the reload
/// itself succeeded and returns `200`.
pub(crate) async fn reload_all(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let report = state
        .mcp
        .reload_from_disk()
        .await
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;
    Ok(Json(json!({ "reload": report })))
}

/// `POST /api/mcp/servers/{name}/refresh`: re-discovers one connected
/// server's tools. Unknown names are `404`; a discovery failure is `502`.
pub(crate) async fn refresh_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if state.mcp.tools(&name).await.is_none() {
        return Err(ApiError::not_found(format!("MCP server {name:?}")));
    }
    let tools = state
        .mcp
        .refresh(&name)
        .await
        .map_err(|error| ApiError::bad_gateway(format!("{error:#}")))?;
    Ok(Json(json!({ "server": name, "tools": tools })))
}

fn json_body(payload: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiError> {
    payload
        .map(|Json(value)| value)
        .map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

fn config_path(state: &AppState) -> Result<PathBuf, ApiError> {
    state.mcp.config_path().ok_or_else(|| {
        ApiError::bad_request(
            "no MCP config path is configured; set DHI_MCP_CONFIG or place \
             ops/mcp.json in the working tree",
        )
    })
}

/// Parses `{ "name": "...", ...entry }`. The name is optional on update,
/// where the path is authoritative, and required on add.
fn parse_entry_body(
    body: Value,
    path_name: Option<&str>,
) -> Result<(String, dhi_mcp::McpServerEntry), ApiError> {
    let Value::Object(mut object) = body else {
        return Err(ApiError::bad_request(
            "MCP server body must be a JSON object",
        ));
    };
    let body_name = match object.remove("name") {
        Some(Value::String(name)) => Some(name.trim().to_string()),
        Some(_) => {
            return Err(ApiError::bad_request(
                "MCP server \"name\" must be a string",
            ));
        }
        None => None,
    };
    let name = match (path_name, body_name) {
        (Some(path), Some(body)) if body != path.trim() => {
            return Err(ApiError::bad_request(format!(
                "MCP server body name {body:?} does not match the path name {path:?}; \
                 rename by deleting and adding the server"
            )));
        }
        (Some(path), _) => path.trim().to_string(),
        (None, Some(body)) => body,
        (None, None) => {
            return Err(ApiError::bad_request(
                "MCP server body must include a \"name\"",
            ));
        }
    };
    if name.is_empty() {
        return Err(ApiError::bad_request("MCP server name must not be empty"));
    }
    let entry: dhi_mcp::McpServerEntry = serde_json::from_value(Value::Object(object))
        .map_err(|error| ApiError::bad_request(format!("invalid MCP server entry: {error}")))?;
    dhi_mcp::edit::validate_entry(&name, &entry)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok((name, entry))
}

/// Persists first, then reloads. A target connect failure is `502`; a reload
/// failure after a successful write is `500` (the file is already saved).
async fn reload_after_write(
    state: &AppState,
    target: Option<&str>,
) -> Result<ReloadReport, ApiError> {
    let report = state.mcp.reload_from_disk().await.map_err(|error| {
        ApiError::internal(format!("saved the MCP config but reload failed: {error:#}"))
    })?;
    if let Some(name) = target
        && let Some(failure) = report.failure_for(name)
    {
        return Err(ApiError::bad_gateway(format!(
            "MCP server {name:?} was saved but did not connect: {}",
            failure.error
        )));
    }
    Ok(report)
}

fn summary_for(name: &str, entry: &dhi_mcp::McpServerEntry) -> Result<ServerSummary, ApiError> {
    let validated = entry.validate(name).map_err(|error| {
        ApiError::internal(format!(
            "validated MCP server {name:?} no longer validates: {error}"
        ))
    })?;
    let server = ValidatedServer::try_from(validated).map_err(|error| {
        ApiError::internal(format!(
            "validated MCP server {name:?} cannot be used: {error}"
        ))
    })?;
    let mut summary = summarize(&server);
    summary.enabled = entry.enabled;
    Ok(summary)
}

/// Maps the config editor's errors to the API's status codes.
fn edit_error(error: dhi_mcp::EditError) -> ApiError {
    let message = error.to_string();
    match error {
        dhi_mcp::EditError::Duplicate(_) => ApiError::conflict(message),
        dhi_mcp::EditError::Unknown(_) => ApiError::not_found(message),
        dhi_mcp::EditError::Invalid(_) => ApiError::bad_request(message),
    }
}
