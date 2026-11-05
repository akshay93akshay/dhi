//! A tiny scripted HTTP server shared by the offline integration tests.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct StubResponse {
    status: StatusCode,
    body: String,
    delay: Duration,
    headers: Vec<(String, String)>,
}

impl StubResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self {
            status: StatusCode::from_u16(status).expect("valid status"),
            body: body.to_string(),
            delay: Duration::ZERO,
            headers: Vec::new(),
        }
    }

    pub fn raw(status: u16, body: impl Into<String>) -> Self {
        Self {
            status: StatusCode::from_u16(status).expect("valid status"),
            body: body.into(),
            delay: Duration::ZERO,
            headers: Vec::new(),
        }
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FOUND,
            body: String::new(),
            delay: Duration::ZERO,
            headers: vec![("location".to_string(), location.into())],
        }
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn query_param(&self, name: &str) -> Option<String> {
        let query = self.query.as_deref()?;
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }
}

#[derive(Clone)]
struct StubState {
    responses: Arc<Mutex<VecDeque<StubResponse>>>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// A handle for inspecting requests seen by a stub server.
#[derive(Clone)]
pub struct Recorder {
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Recorder {
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded
            .lock()
            .expect("recorded mutex poisoned")
            .clone()
    }

    pub fn count(&self) -> usize {
        self.recorded.lock().expect("recorded mutex poisoned").len()
    }
}

/// Starts a server that answers requests from `responses` in order.
pub async fn spawn_stub(responses: impl IntoIterator<Item = StubResponse>) -> (String, Recorder) {
    let responses = Arc::new(Mutex::new(responses.into_iter().collect::<VecDeque<_>>()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let state = StubState {
        responses,
        recorded: recorded.clone(),
    };
    let app = Router::new().fallback(handle).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub server");
    let addr = listener.local_addr().expect("stub address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("stub server");
    });
    (format!("http://{addr}"), Recorder { recorded })
}

async fn handle(
    State(state): State<StubState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    let headers = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    state
        .recorded
        .lock()
        .expect("recorded mutex poisoned")
        .push(RecordedRequest {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().map(str::to_string),
            headers,
            body,
        });
    let response = state
        .responses
        .lock()
        .expect("responses mutex poisoned")
        .pop_front()
        .unwrap_or_else(|| StubResponse::json(500, json!({"error": "stub exhausted"})));
    if !response.delay.is_zero() {
        tokio::time::sleep(response.delay).await;
    }
    let mut reply = (response.status, response.body).into_response();
    reply.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    for (name, value) in response.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            reply.headers_mut().insert(name, value);
        }
    }
    reply
}
