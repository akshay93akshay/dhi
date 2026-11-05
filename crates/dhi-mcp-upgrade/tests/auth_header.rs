use dhi_mcp_upgrade::testkit::spawn_auth_mcp;
use reqwest::header::ACCEPT;
use serde_json::{Value, json};

fn request(method: &str, id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {}
    })
}

async fn post(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    accept: Option<&str>,
    body: &Value,
) -> reqwest::Response {
    let mut builder = http.post(url).json(body);
    if let Some(token) = token {
        builder = builder.bearer_auth(token);
    }
    if let Some(accept) = accept {
        builder = builder.header(ACCEPT, accept);
    }
    builder.send().await.expect("fake auth request")
}

#[tokio::test]
async fn fake_endpoint_requires_token_accept_and_dispatches_methods() {
    let server = spawn_auth_mcp("test-token").await;
    let url = format!("{}/mcp", server.base_url);
    let http = reqwest::Client::new();
    let accept = "application/json, text/event-stream";

    let initialize = post(
        &http,
        &url,
        Some("test-token"),
        Some(accept),
        &request("initialize", 1),
    )
    .await;
    assert_eq!(initialize.status(), reqwest::StatusCode::OK);
    let body: Value = initialize.json().await.expect("JSON-RPC body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["result"]["serverInfo"]["name"], "auth-fake");

    let list = post(
        &http,
        &url,
        Some("test-token"),
        Some(accept),
        &request("tools/list", 2),
    )
    .await;
    assert_eq!(list.status(), reqwest::StatusCode::OK);
    let body: Value = list.json().await.expect("tools/list body");
    assert_eq!(body["result"]["tools"][0]["name"], "echo");

    let call = post(
        &http,
        &url,
        Some("test-token"),
        Some(accept),
        &request("tools/call", 3),
    )
    .await;
    assert_eq!(call.status(), reqwest::StatusCode::OK);
    let body: Value = call.json().await.expect("tools/call body");
    assert_eq!(body["result"]["content"][0]["text"], "authorized");

    let missing = post(&http, &url, None, Some(accept), &request("initialize", 4)).await;
    assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);
    let missing_body = missing.text().await.expect("unauthorized body");
    assert!(!missing_body.contains("test-token"), "{missing_body}");

    let wrong = post(
        &http,
        &url,
        Some("wrong-token"),
        Some(accept),
        &request("initialize", 5),
    )
    .await;
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

    let missing_accept = post(
        &http,
        &url,
        Some("test-token"),
        None,
        &request("initialize", 6),
    )
    .await;
    assert_eq!(missing_accept.status(), reqwest::StatusCode::NOT_ACCEPTABLE);
}
