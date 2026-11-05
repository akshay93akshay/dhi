//! Tool catalog and control routes.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use dhi_tool_control::{ToolControlAction, ToolControlError};
use serde_json::{Value, json};

use crate::{ApiError, AppState};

/// `GET /api/tools/catalog`: every registered tool, with control overrides
/// applied when the management store is available.
pub(crate) async fn catalog(State(state): State<AppState>) -> Json<Value> {
    let mut build = state.tool_catalog.snapshot();
    let mut control_error = None;
    if let Some(control) = &state.tool_control {
        match control.states().await {
            Ok(states) => {
                let states: BTreeMap<&str, &dhi_tool_control::ToolState> = states
                    .iter()
                    .map(|state| (state.key.as_str(), state))
                    .collect();
                for tool in &mut build.snapshot.tools {
                    if let Some(state) = states.get(tool.key.as_str()) {
                        tool.enabled = state.enabled;
                        tool.default_roles = state.roles.clone();
                    }
                }
            }
            Err(error) => control_error = Some(error.to_string()),
        }
    }
    Json(json!({
        "tools": build.snapshot.tools,
        "errors": build.errors,
        "control_error": control_error,
    }))
}

/// `GET /api/tools/control`: current enabled/visibility state.
pub(crate) async fn states(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let control = control(&state)?;
    let states = control.states().await.map_err(map_control)?;
    Ok(Json(json!({ "tools": states })))
}

/// `POST /api/tools/control`: apply one control action.
pub(crate) async fn apply(
    State(state): State<AppState>,
    Json(action): Json<ToolControlAction>,
) -> Result<Json<Value>, ApiError> {
    let control = control(&state)?;
    let states = control.apply(action).await.map_err(map_control)?;
    Ok(Json(json!({ "tools": states })))
}

fn control(
    state: &AppState,
) -> Result<&std::sync::Arc<dyn dhi_tool_control::ToolControl>, ApiError> {
    state
        .tool_control
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("tool control store is not available"))
}

fn map_control(error: ToolControlError) -> ApiError {
    match error {
        ToolControlError::UnknownTool(message) => ApiError::not_found(message),
        ToolControlError::Invalid(message) => ApiError::bad_request(message),
        ToolControlError::Store(message) => ApiError::internal(message),
    }
}
