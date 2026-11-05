//! Stable DHI tool definitions and JSON argument parsing.
//!
//! The three tool names never change even when the configured provider does.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::WebError;
use crate::model::{
    DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT, ReadRequest, ResearchDepth, ResearchRequest,
    SearchRequest,
};

pub const WEB_TOOL_NAMES: [&str; 3] = ["web_search", "web_read", "web_research"];

/// One model-facing tool definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// The three stable DHI web tools.
pub fn web_tool_specs() -> Vec<WebToolSpec> {
    vec![
        WebToolSpec {
            name: "web_search".to_string(),
            description: "Search the web. Results are normalized across the configured \
                          providers and returned as {title, url, snippet, provider}."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SEARCH_LIMIT,
                        "description": format!("How many results to return (1-{MAX_SEARCH_LIMIT}, default {DEFAULT_SEARCH_LIMIT}).")
                    },
                    "objective": {
                        "type": "string",
                        "description": "Optional natural-language goal guiding the search."
                    },
                    "queries": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional keyword queries for providers that support them."
                    },
                    "recency_minutes": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional freshness window in minutes."
                    },
                    "include_domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional domains to include."
                    },
                    "exclude_domains": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional domains to exclude."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        WebToolSpec {
            name: "web_read".to_string(),
            description: "Read one web page or public PDF/DOCX URL and return its Markdown \
                          (capped, with truncation stated)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "An http(s) page or document URL."
                    },
                    "objective": {
                        "type": "string",
                        "description": "Optional question the extractor should focus on."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
        WebToolSpec {
            name: "web_research".to_string(),
            description: "Answer a question with web research and return an answer plus \
                          citation URLs."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The research question."
                    },
                    "depth": {
                        "type": "string",
                        "enum": ["quick", "standard", "deep"],
                        "description": "Research effort; default standard."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        },
    ]
}

pub fn parse_search_input(input: &Value) -> Result<SearchRequest, WebError> {
    let object = input_object("web_search", input)?;
    let query = required_string("web_search", object, "query")?;
    let limit = match object.get("limit") {
        None | Some(Value::Null) => DEFAULT_SEARCH_LIMIT,
        Some(value) => {
            let raw = value.as_u64().ok_or_else(|| {
                WebError::invalid_input(format!(
                    "web_search: limit must be an integer between 1 and {MAX_SEARCH_LIMIT}"
                ))
            })?;
            usize::try_from(raw).map_err(|_| {
                WebError::invalid_input(format!(
                    "web_search: limit must be an integer between 1 and {MAX_SEARCH_LIMIT}"
                ))
            })?
        }
    };
    let objective = optional_string("web_search", object, "objective")?;
    let queries = string_array("web_search", object, "queries")?;
    let recency_minutes = match object.get("recency_minutes") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            WebError::invalid_input("web_search: recency_minutes must be a positive integer")
        })?),
    };
    let include_domains = string_array("web_search", object, "include_domains")?;
    let exclude_domains = string_array("web_search", object, "exclude_domains")?;
    Ok(SearchRequest {
        query,
        limit,
        objective,
        queries,
        recency_minutes,
        include_domains,
        exclude_domains,
    })
}

pub fn parse_read_input(input: &Value) -> Result<ReadRequest, WebError> {
    let object = input_object("web_read", input)?;
    Ok(ReadRequest {
        url: required_string("web_read", object, "url")?,
        objective: optional_string("web_read", object, "objective")?,
    })
}

pub fn parse_research_input(input: &Value) -> Result<ResearchRequest, WebError> {
    let object = input_object("web_research", input)?;
    let question = required_string("web_research", object, "question")?;
    let depth = match optional_string("web_research", object, "depth")? {
        Some(raw) => ResearchDepth::from_str(&raw)?,
        None => ResearchDepth::default(),
    };
    Ok(ResearchRequest { question, depth })
}

fn input_object<'a>(tool: &str, input: &'a Value) -> Result<&'a Map<String, Value>, WebError> {
    input
        .as_object()
        .ok_or_else(|| WebError::invalid_input(format!("{tool}: input must be a JSON object")))
}

fn required_string(tool: &str, object: &Map<String, Value>, key: &str) -> Result<String, WebError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WebError::invalid_input(format!("{tool}: {key} must be a non-empty string")))
}

fn optional_string(
    tool: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, WebError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            Ok((!value.is_empty()).then(|| value.to_string()))
        }
        Some(_) => Err(WebError::invalid_input(format!(
            "{tool}: {key} must be a string when present"
        ))),
    }
}

fn string_array(
    tool: &str,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, WebError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .map(str::to_string)
                    .ok_or_else(|| {
                        WebError::invalid_input(format!("{tool}: {key} must contain only strings"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|values| {
                values
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .collect()
            }),
        Some(_) => Err(WebError::invalid_input(format!(
            "{tool}: {key} must be an array of strings when present"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_stable() {
        let names = web_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, WEB_TOOL_NAMES);
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(parse_search_input(&json!({})).is_err());
        assert!(parse_search_input(&json!({"query": "x", "limit": "five"})).is_err());
        assert!(parse_read_input(&json!({"url": 7})).is_err());
        assert!(parse_research_input(&json!({"question": "x", "depth": "huge"})).is_err());
    }

    #[test]
    fn parses_full_search_request() {
        let request = parse_search_input(&json!({
            "query": "rust",
            "limit": 3,
            "objective": "find releases",
            "queries": ["rust 2026"],
            "recency_minutes": 60,
            "include_domains": ["rust-lang.org"],
            "exclude_domains": ["example.com"]
        }))
        .expect("valid");
        assert_eq!(request.limit, 3);
        assert_eq!(request.queries, ["rust 2026"]);
        assert_eq!(request.recency_minutes, Some(60));
    }
}
