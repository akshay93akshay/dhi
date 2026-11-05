use std::collections::BTreeMap;
use std::time::Duration;

use dhi_mcp_upgrade::legacy_sse::{LegacySseClient, LegacySseConfig, LegacySseError};
use dhi_mcp_upgrade::testkit::spawn_sse_server;
use serde_json::json;

fn required_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_string(), "Bearer test-token".to_string()),
        ("X-Test".to_string(), "custom".to_string()),
    ])
}

fn config(base_url: &str, path: &str) -> LegacySseConfig {
    let mut config = LegacySseConfig::new(format!("{base_url}{path}"))
        .with_bearer_token("test-token")
        .with_header("X-Test", "custom");
    config.handshake_timeout = Duration::from_secs(2);
    config.call_timeout = Duration::from_secs(2);
    config.discovery_timeout = Duration::from_secs(5);
    config
}

#[tokio::test]
async fn lists_and_calls_tools_over_legacy_sse() {
    let server = spawn_sse_server(required_headers()).await;
    let client = LegacySseClient::connect(config(&server.base_url, "/sse"))
        .await
        .expect("connect to fake SSE server");

    assert_eq!(
        client.endpoint(),
        format!("{}/messages?session=fake", server.base_url)
    );

    let tools = client.list_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["echo", "add"]);
    assert_eq!(tools[0].description_text(), "Echo the supplied arguments");

    let result = client
        .call_tool("echo", json!({"text": "hello"}))
        .await
        .expect("call tool");
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(text.contains("echo"), "{text}");
    assert!(text.contains("hello"), "{text}");
}

#[tokio::test]
async fn direct_json_post_responses_are_used() {
    let server = spawn_sse_server(required_headers()).await;
    let client = LegacySseClient::connect(config(&server.base_url, "/sse-post-json"))
        .await
        .expect("connect via direct JSON POST responses");

    let tools = client.list_tools().await.expect("list tools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["echo", "add"]
    );
    let result = client
        .call_tool("add", json!({"a": 1, "b": 2}))
        .await
        .expect("call tool");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("add")
    );
}

#[tokio::test]
async fn unexpected_post_content_type_is_an_error() {
    let server = spawn_sse_server(required_headers()).await;
    let error = LegacySseClient::connect(config(&server.base_url, "/sse-post-unexpected"))
        .await
        .expect_err("unexpected POST content type");
    assert!(
        matches!(error, LegacySseError::UnexpectedContentType { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn missing_or_wrong_authorization_is_rejected() {
    let server = spawn_sse_server(required_headers()).await;

    let mut missing = config(&server.base_url, "/sse");
    missing.headers.remove("Authorization");
    let error = LegacySseClient::connect(missing)
        .await
        .expect_err("missing bearer token");
    assert!(
        matches!(error, LegacySseError::Http { status: 401, .. }),
        "{error:?}"
    );
    assert!(!error.to_string().contains("test-token"), "{error}");

    let mut wrong = config(&server.base_url, "/sse");
    wrong.headers.insert(
        "Authorization".to_string(),
        "Bearer wrong-token".to_string(),
    );
    let error = LegacySseClient::connect(wrong)
        .await
        .expect_err("wrong bearer token");
    assert!(
        matches!(error, LegacySseError::Http { status: 401, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn handshake_timeout_is_bounded() {
    let server = spawn_sse_server(required_headers()).await;
    let mut config = config(&server.base_url, "/sse-silent-endpoint");
    config.handshake_timeout = Duration::from_millis(100);

    let error = LegacySseClient::connect(config)
        .await
        .expect_err("no endpoint event");
    assert!(
        matches!(error, LegacySseError::HandshakeTimeout { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn call_timeout_is_bounded() {
    let server = spawn_sse_server(required_headers()).await;
    let mut config = config(&server.base_url, "/sse-silent-calls");
    config.call_timeout = Duration::from_millis(100);

    let client = LegacySseClient::connect(config)
        .await
        .expect("connect to fake SSE server");
    let error = client
        .call_tool("echo", json!({"text": "never answered"}))
        .await
        .expect_err("call has no response");
    assert!(
        matches!(error, LegacySseError::CallTimeout { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn dropped_call_does_not_break_the_next_correlation() {
    let server = spawn_sse_server(required_headers()).await;
    let client = LegacySseClient::connect(config(&server.base_url, "/sse-delayed-calls"))
        .await
        .expect("connect to fake SSE server");

    let first = tokio::time::timeout(
        Duration::from_millis(50),
        client.call_tool("echo", json!({"text": "too slow"})),
    )
    .await;
    assert!(first.is_err(), "short outer timeout must cancel the call");

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        client.call_tool("add", json!({"a": 1, "b": 2})),
    )
    .await
    .expect("second call did not hang")
    .expect("second call correlates");
    let text = second["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("add"), "{text}");
}

#[tokio::test]
async fn unknown_tool_is_a_jsonrpc_error() {
    let server = spawn_sse_server(required_headers()).await;
    let client = LegacySseClient::connect(config(&server.base_url, "/sse"))
        .await
        .expect("connect to fake SSE server");

    let error = client
        .call_tool("missing", json!({}))
        .await
        .expect_err("unknown tool");
    assert!(
        matches!(error, LegacySseError::Rpc { code: -32602, .. }),
        "{error:?}"
    );
}
