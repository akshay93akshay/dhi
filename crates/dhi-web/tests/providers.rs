mod common;

use std::time::Duration;

use common::{StubResponse, spawn_stub};
use dhi_web::{
    ErrorKind, FirecrawlConfig, FirecrawlProvider, MAX_CITATIONS, MAX_DOCUMENT_CHARS,
    MAX_RESEARCH_ANSWER_CHARS, MAX_RESULT_URL_CHARS, MAX_SNIPPET_CHARS, MAX_TITLE_CHARS,
    ParallelConfig, ParallelProvider, ReadRequest, ResearchDepth, ResearchRequest, SearchRequest,
    Secret, TinyFishConfig, TinyFishProvider, WebProvider,
};
use serde_json::json;

fn secret(raw: &str) -> Secret {
    Secret::new(raw).expect("non-empty secret")
}

fn tinyfish(base: &str, timeout: Duration) -> TinyFishProvider {
    TinyFishProvider::new(
        TinyFishConfig::new(secret("tf-secret"), timeout)
            .with_search_url(format!("{base}/search"))
            .with_fetch_url(format!("{base}/fetch"))
            .with_allow_private_targets(true),
    )
    .expect("tinyfish provider")
}

fn parallel(base: &str, timeout: Duration) -> ParallelProvider {
    ParallelProvider::new(
        ParallelConfig::new(secret("par-secret"), timeout)
            .with_base_url(base)
            .with_allow_private_targets(true),
    )
    .expect("parallel provider")
}

#[tokio::test]
async fn tinyfish_search_sends_expected_request_and_normalizes_results() {
    let long_title = "t".repeat(MAX_TITLE_CHARS + 50);
    let long_snippet = "s".repeat(MAX_SNIPPET_CHARS + 50);
    let (base, recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "query": "rust lang",
            "results": [
                {
                    "position": 1,
                    "site_name": "rust-lang.org",
                    "title": long_title,
                    "snippet": long_snippet,
                    "url": "https://www.rust-lang.org/"
                },
                {
                    "position": 2,
                    "title": "Second",
                    "snippet": "not requested",
                    "url": "https://example.com/second"
                }
            ],
            "total_results": 2,
            "page": 0
        }),
    )])
    .await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let request = SearchRequest::new("rust lang")
        .limit(1)
        .objective("find the official docs")
        .recency_minutes(60)
        .include_domains(["rust-lang.org"])
        .exclude_domains(["example.com"]);
    let results = provider.search(&request).await.expect("search");

    assert_eq!(results.len(), 1);
    assert!(results[0].title.chars().count() <= MAX_TITLE_CHARS + 1);
    assert!(results[0].title.ends_with('…'));
    assert!(
        results[0].snippet.chars().count() <= MAX_SNIPPET_CHARS + 1,
        "snippet should be capped"
    );
    assert_eq!(results[0].url, "https://www.rust-lang.org/");
    assert_eq!(results[0].provider, "tinyfish");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/search");
    assert_eq!(request.header("x-api-key"), Some("tf-secret"));
    assert_eq!(request.query_param("query").as_deref(), Some("rust lang"));
    assert_eq!(
        request.query_param("purpose").as_deref(),
        Some("find the official docs")
    );
    assert_eq!(
        request.query_param("recency_minutes").as_deref(),
        Some("60")
    );
    assert_eq!(
        request.query_param("include_domains").as_deref(),
        Some("rust-lang.org")
    );
    assert_eq!(
        request.query_param("exclude_domains").as_deref(),
        Some("example.com")
    );
}

#[tokio::test]
async fn search_result_urls_are_capped() {
    let long_url = format!(
        "https://example.com/{}",
        "u".repeat(MAX_RESULT_URL_CHARS + 200)
    );
    let (base, _) = spawn_stub([StubResponse::json(
        200,
        json!({ "results": [{ "url": long_url, "title": "Long", "snippet": "x" }] }),
    )])
    .await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let results = provider
        .search(&SearchRequest::new("long url"))
        .await
        .expect("search");
    assert!(results[0].url.chars().count() <= MAX_RESULT_URL_CHARS + 1);
}

#[tokio::test]
async fn tinyfish_fetch_sends_fresh_body_and_returns_final_url() {
    let (base, recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [
                {
                    "url": "https://example.com/page",
                    "final_url": "https://example.com/final",
                    "title": "Example",
                    "format": "markdown",
                    "text": "# Example\n\nBody."
                }
            ],
            "errors": []
        }),
    )])
    .await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let result = provider
        .read(&ReadRequest::new("https://example.com/page"))
        .await
        .expect("read");

    assert_eq!(result.url, "https://example.com/final");
    assert_eq!(result.title, "Example");
    assert_eq!(result.markdown, "# Example\n\nBody.");
    assert!(!result.truncated);
    assert_eq!(result.status_code, None);
    assert_eq!(result.provider, "tinyfish");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/fetch");
    assert_eq!(requests[0].header("x-api-key"), Some("tf-secret"));
    assert_eq!(
        requests[0].body,
        json!({
            "urls": ["https://example.com/page"],
            "format": "markdown",
            "ttl": 0
        })
    );
}

#[tokio::test]
async fn tinyfish_per_url_errors_are_fallback_eligible() {
    let (base, _) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [],
            "errors": [{ "url": "https://example.com/page", "error": "bot blocked" }]
        }),
    )])
    .await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let error = provider
        .read(&ReadRequest::new("https://example.com/page"))
        .await
        .expect_err("per-url failure");
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert!(error.is_fallback());
    assert!(error.message().contains("bot blocked"), "{error}");
}

#[tokio::test]
async fn parallel_search_sends_objective_and_queries_and_normalizes_excerpts() {
    let (base, recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "search_id": "search_1",
            "results": [
                {
                    "url": "https://example.com/a",
                    "title": "A",
                    "excerpts": ["first excerpt", "second excerpt"]
                },
                {
                    "url": "https://example.com/b",
                    "title": null,
                    "excerpts": ["other"]
                }
            ],
            "session_id": "session_1"
        }),
    )])
    .await;
    let provider = parallel(&base, Duration::from_secs(5));
    let request = SearchRequest::new("fallback query")
        .limit(2)
        .objective("compare providers")
        .queries(["parallel search", "web api"])
        .include_domains(["example.com"]);
    let results = provider.search(&request).await.expect("search");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "A");
    assert_eq!(
        results[0].snippet, "first excerpt\n\nsecond excerpt",
        "excerpts are joined"
    );
    assert_eq!(results[1].title, "");
    assert_eq!(results[0].provider, "parallel");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/search");
    assert_eq!(requests[0].header("x-api-key"), Some("par-secret"));
    assert_eq!(requests[0].body["objective"], "compare providers");
    assert_eq!(
        requests[0].body["search_queries"],
        json!(["parallel search", "web api"])
    );
    assert_eq!(requests[0].body["advanced_settings"]["max_results"], 2);
    assert_eq!(
        requests[0].body["advanced_settings"]["source_policy"]["include_domains"],
        json!(["example.com"])
    );
}

#[tokio::test]
async fn parallel_extract_requests_full_content_and_truncates_long_documents() {
    let long = "x".repeat(MAX_DOCUMENT_CHARS + 500);
    let (base, recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "extract_id": "extract_1",
            "results": [
                {
                    "url": "https://example.com/final",
                    "title": "Example",
                    "full_content": long,
                    "excerpts": ["ignored excerpt"]
                }
            ],
            "errors": [],
            "session_id": "session_1"
        }),
    )])
    .await;
    let provider = parallel(&base, Duration::from_secs(5));
    let result = provider
        .read(&ReadRequest::new("https://example.com/page").objective("summarize"))
        .await
        .expect("read");

    assert_eq!(result.url, "https://example.com/final");
    assert_eq!(result.title, "Example");
    assert_eq!(result.markdown.chars().count(), MAX_DOCUMENT_CHARS);
    assert!(result.truncated);
    assert_eq!(result.provider, "parallel");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/extract");
    assert_eq!(requests[0].header("x-api-key"), Some("par-secret"));
    assert_eq!(
        requests[0].body["urls"],
        json!(["https://example.com/page"])
    );
    assert_eq!(requests[0].body["objective"], "summarize");
    assert_eq!(requests[0].body["advanced_settings"]["full_content"], true);
    assert_eq!(
        requests[0].body["advanced_settings"]["fetch_policy"]["max_age_seconds"],
        600
    );
    assert_eq!(
        requests[0].body["advanced_settings"]["fetch_policy"]["disable_cache_fallback"],
        true
    );
}

#[tokio::test]
async fn parallel_extract_null_status_errors_are_fallback_eligible() {
    let (base, _) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [],
            "errors": [{
                "url": "https://example.com/page",
                "error_type": "fetch_error",
                "http_status_code": null,
                "content": null
            }]
        }),
    )])
    .await;
    let provider = parallel(&base, Duration::from_secs(5));
    let error = provider
        .read(&ReadRequest::new("https://example.com/page"))
        .await
        .expect_err("fetch error");
    assert_eq!(error.kind, ErrorKind::Upstream);
    assert!(error.is_fallback());
    assert!(error.message().contains("fetch_error"), "{error}");
}

#[tokio::test]
async fn parallel_research_uses_task_api_and_deduplicates_citations() {
    let (base, recorder) = spawn_stub([
        StubResponse::json(
            202,
            json!({
                "run_id": "trun_1",
                "status": "queued",
                "is_active": true,
                "processor": "pro"
            }),
        ),
        StubResponse::json(
            200,
            json!({
                "run": { "run_id": "trun_1", "status": "completed" },
                "output": {
                    "type": "text",
                    "content": "A sourced answer.",
                    "basis": [
                        { "field": "output", "citations": [
                            { "url": "https://example.com/a" },
                            { "url": "https://example.com/b" }
                        ]},
                        { "field": "output", "citations": [
                            { "url": "https://example.com/a" }
                        ]}
                    ]
                }
            }),
        ),
    ])
    .await;
    let provider = parallel(&base, Duration::from_secs(5));
    let result = provider
        .research(&ResearchRequest::new("what happened?").depth(ResearchDepth::Deep))
        .await
        .expect("research");

    assert_eq!(result.answer, "A sourced answer.");
    assert_eq!(
        result.citations,
        ["https://example.com/a", "https://example.com/b"]
    );
    assert!(!result.truncated);
    assert_eq!(result.provider, "parallel");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/tasks/runs");
    assert_eq!(requests[0].header("x-api-key"), Some("par-secret"));
    assert_eq!(requests[0].body["input"], "what happened?");
    assert_eq!(requests[0].body["processor"], "pro");

    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].path, "/v1/tasks/runs/trun_1/result");
    assert_eq!(requests[1].header("x-api-key"), Some("par-secret"));
    let timeout = requests[1]
        .query_param("timeout")
        .expect("timeout query")
        .parse::<u64>()
        .expect("numeric timeout");
    assert!((899..=900).contains(&timeout), "timeout={timeout}");
}

#[tokio::test]
async fn parallel_research_retries_408_and_preserves_run_id() {
    let (base, recorder) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_retry" })),
        StubResponse::json(408, json!({ "error": "still active" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": { "type": "text", "content": "Eventually done.", "basis": [] }
            }),
        ),
    ])
    .await;
    let provider = ParallelProvider::new(
        ParallelConfig::new(secret("par-secret"), Duration::from_secs(5))
            .with_base_url(&base)
            .with_allow_private_targets(true)
            .with_research_timeout(Duration::from_secs(5))
            .with_research_poll_interval(Duration::from_millis(10)),
    )
    .expect("provider");
    let result = provider
        .research(&ResearchRequest::new("retry me"))
        .await
        .expect("research");
    assert_eq!(result.answer, "Eventually done.");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/v1/tasks/runs");
    assert_eq!(requests[1].path, "/v1/tasks/runs/trun_retry/result");
    assert_eq!(requests[2].path, "/v1/tasks/runs/trun_retry/result");
    for request in &requests[1..] {
        let timeout = request
            .query_param("timeout")
            .expect("timeout query")
            .parse::<u64>()
            .expect("numeric timeout");
        assert!((1..=5).contains(&timeout), "timeout={timeout}");
    }
}

#[tokio::test]
async fn parallel_research_timeout_is_reported() {
    let (base, recorder) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_slow" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": { "type": "text", "content": "too late", "basis": [] }
            }),
        )
        .delay(Duration::from_millis(400)),
    ])
    .await;
    let provider = ParallelProvider::new(
        ParallelConfig::new(secret("par-secret"), Duration::from_secs(5))
            .with_base_url(&base)
            .with_allow_private_targets(true)
            .with_research_timeout(Duration::from_millis(100))
            .with_research_poll_interval(Duration::from_millis(10)),
    )
    .expect("provider");
    let started = std::time::Instant::now();
    let error = provider
        .research(&ResearchRequest::new("slow"))
        .await
        .expect_err("timeout");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert!(error.message().contains("trun_slow"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(recorder.count(), 2);
}

#[tokio::test]
async fn parallel_research_caps_answer_and_citations() {
    let long_answer = "a".repeat(MAX_RESEARCH_ANSWER_CHARS + 100);
    let citations = (0..MAX_CITATIONS + 10)
        .map(|index| json!({ "url": format!("https://example.com/{index}") }))
        .collect::<Vec<_>>();
    let (base, _) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_big" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": {
                    "type": "text",
                    "content": long_answer,
                    "basis": [{ "citations": citations }]
                }
            }),
        ),
    ])
    .await;
    let provider = parallel(&base, Duration::from_secs(5));
    let result = provider
        .research(&ResearchRequest::new("huge"))
        .await
        .expect("research");
    assert_eq!(result.answer.chars().count(), MAX_RESEARCH_ANSWER_CHARS);
    assert!(result.truncated);
    assert_eq!(result.citations.len(), MAX_CITATIONS);
}

#[tokio::test]
async fn firecrawl_search_and_read_use_filters_freshness_and_final_url() {
    let (base, recorder) = spawn_stub([
        StubResponse::json(
            200,
            json!({
                "success": true,
                "data": {
                    "web": [
                        {
                            "url": "https://example.com/result",
                            "title": "Result",
                            "description": "A compact description"
                        }
                    ]
                }
            }),
        ),
        StubResponse::json(
            200,
            json!({
                "success": true,
                "data": {
                    "markdown": "# Page\n\nContent.",
                    "metadata": {
                        "title": "Page",
                        "statusCode": 200,
                        "url": "https://example.com/final"
                    }
                }
            }),
        ),
        StubResponse::json(
            200,
            json!({
                "success": true,
                "data": {
                    "markdown": "# PDF\n\nParsed.",
                    "metadata": { "title": "PDF", "statusCode": 200 }
                }
            }),
        ),
    ])
    .await;
    let provider = FirecrawlProvider::new(
        FirecrawlConfig::new(&base, Some(secret("fc-secret")), Duration::from_secs(5))
            .with_allow_private_targets(true),
    )
    .expect("provider");

    let results = provider
        .search(
            &SearchRequest::new("example")
                .limit(3)
                .include_domains(["a.example"])
                .exclude_domains(["b.example"])
                .recency_minutes(120),
        )
        .await
        .expect("search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].provider, "firecrawl");

    let read = provider
        .read(&ReadRequest::new("https://example.com/page"))
        .await
        .expect("read");
    assert_eq!(read.url, "https://example.com/final");
    assert_eq!(read.title, "Page");
    assert_eq!(read.markdown, "# Page\n\nContent.");
    assert_eq!(read.status_code, Some(200));
    assert!(!read.truncated);

    let pdf = provider
        .read(&ReadRequest::new("https://example.com/report.pdf"))
        .await
        .expect("read pdf");
    assert_eq!(pdf.markdown, "# PDF\n\nParsed.");

    let requests = recorder.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/v2/search");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer fc-secret")
    );
    assert_eq!(requests[0].body["query"], "example");
    assert_eq!(requests[0].body["includeDomains"], json!(["a.example"]));
    assert_eq!(requests[0].body["excludeDomains"], json!(["b.example"]));
    assert_eq!(requests[0].body["tbs"], "qdr:d");

    assert_eq!(requests[1].path, "/v2/scrape");
    assert_eq!(requests[1].body["url"], "https://example.com/page");
    assert_eq!(requests[1].body["formats"], json!(["markdown"]));
    assert_eq!(requests[1].body["maxAge"], 0);
    assert!(requests[1].body.get("parsers").is_none());

    assert_eq!(requests[2].body["url"], "https://example.com/report.pdf");
    assert_eq!(
        requests[2].body["parsers"],
        json!([{ "type": "pdf", "mode": "auto" }])
    );
}

#[tokio::test]
async fn firecrawl_empty_and_success_false_reads_are_fallback_eligible() {
    let (base, _) = spawn_stub([
        StubResponse::json(
            200,
            json!({ "success": false, "error": "blocked by bot protection" }),
        ),
        StubResponse::json(
            200,
            json!({
                "success": true,
                "data": { "markdown": "", "metadata": { "statusCode": 200 } }
            }),
        ),
    ])
    .await;
    let provider = FirecrawlProvider::new(
        FirecrawlConfig::new(&base, Some(secret("fc-secret")), Duration::from_secs(5))
            .with_allow_private_targets(true),
    )
    .expect("provider");
    let first = provider
        .read(&ReadRequest::new("https://example.com/one"))
        .await
        .expect_err("success false");
    assert_eq!(first.kind, ErrorKind::Upstream);
    assert!(first.is_fallback());
    let second = provider
        .read(&ReadRequest::new("https://example.com/two"))
        .await
        .expect_err("empty content");
    assert_eq!(second.kind, ErrorKind::Upstream);
    assert!(second.is_fallback());
}

#[tokio::test]
async fn redirects_are_not_followed_and_do_not_leak_auth_headers() {
    let (target_base, target_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let (redirect_base, redirect_recorder) =
        spawn_stub([StubResponse::redirect(format!("{target_base}/search"))]).await;
    let provider = TinyFishProvider::new(
        TinyFishConfig::new(secret("tf-secret"), Duration::from_secs(5))
            .with_search_url(format!("{redirect_base}/search"))
            .with_fetch_url(format!("{redirect_base}/fetch"))
            .with_allow_private_targets(true),
    )
    .expect("provider");
    let error = provider
        .search(&SearchRequest::new("redirect"))
        .await
        .expect_err("redirects disabled");
    assert_eq!(redirect_recorder.count(), 1);
    assert_eq!(
        target_recorder.count(),
        0,
        "the cross-host redirect must not be followed"
    );
    assert_eq!(error.status, Some(302));
}

#[tokio::test]
async fn oversized_bodies_are_capped_before_parsing_or_reporting() {
    let (base, _) = spawn_stub([StubResponse::raw(200, "x".repeat(4 * 1024 * 1024 + 1_024))]).await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let error = provider
        .search(&SearchRequest::new("huge body"))
        .await
        .expect_err("truncated JSON");
    assert!(
        error.message().len() < 1_000,
        "the body cap must keep the message small"
    );

    let (base, _) = spawn_stub([StubResponse::raw(500, "e".repeat(100_000))]).await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let error = provider
        .search(&SearchRequest::new("huge error"))
        .await
        .expect_err("server error");
    assert_eq!(error.kind, ErrorKind::Server);
    assert!(
        error.message().len() < 1_000,
        "the error-body cap must keep the message small"
    );
}

#[tokio::test]
async fn provider_errors_redact_the_api_key() {
    let (base, _recorder) = spawn_stub([StubResponse::json(
        500,
        json!({ "error": "upstream rejected tf-secret" }),
    )])
    .await;
    let provider = tinyfish(&base, Duration::from_secs(5));
    let error = provider
        .search(&SearchRequest::new("anything"))
        .await
        .expect_err("500");
    assert!(!error.message().contains("tf-secret"), "{error}");
    assert!(error.message().contains("[redacted]"), "{error}");
    assert_eq!(error.kind, ErrorKind::Server);
}
