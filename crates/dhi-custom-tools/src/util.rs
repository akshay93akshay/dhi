//! Shared input validation.

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::config::MAX_INPUT_BYTES;

/// Every custom tool takes one JSON object. The serialized size is capped so
/// a model cannot hand a backend an unbounded body or argument list.
pub(crate) fn object_input<'a>(tool: &str, input: &'a Value) -> Result<&'a Map<String, Value>> {
    let bytes = serde_json::to_vec(input)
        .map_err(|err| anyhow!("custom tool {tool:?} input does not serialize to JSON: {err}"))?;
    if bytes.len() > MAX_INPUT_BYTES {
        bail!(
            "custom tool {tool:?} input is {} bytes; the cap is {MAX_INPUT_BYTES} bytes",
            bytes.len()
        );
    }
    input
        .as_object()
        .ok_or_else(|| anyhow!("custom tool {tool:?} expects a JSON object input"))
}
