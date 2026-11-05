mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{StubResponse, spawn_stub};
use dhi_web::{
    ErrorKind, FirecrawlConfig, FirecrawlProvider, MAX_QUERIES, ParallelConfig, ParallelProvider,
    ReadRequest, ResearchRequest, SearchRequest, Secret, TinyFishConfig, TinyFishProvider,
    WebConfig, WebProvider, WebRouter,
};
use serde_json::json;

fn secret(raw: &str) -> Secret {
    Secret::new(raw).expect("non-empty secret")
}

fn tinyfish(base: &str, timeout: Duration) -> Arc<dyn WebProvider> {
    Arc::new(
        TinyFishProvider::new(
            TinyFishConfig::new(secret("tf-secret"), timeout)
                .with_search_url(format!("{base}/search"))
                .with_fetch_url(format!("{base}/fetch"))
                .with_allow_private_targets(true),
        )
        .expect("tinyfish provider"),
    )
}

fn parallel(base: &str, timeout: Duration) -> Arc<dyn WebProvider> {
    Arc::new(
        ParallelProvider::new(
            ParallelConfig::new(secret("par-secret"), timeout)
                .with_base_url(base)
                .with_allow_private_targets(true),
        )
        .expect("parallel provider"),
    )
}

fn firecrawl(base: &str, timeout: Duration) -> Arc<dyn WebProvider> {
    Arc::new(
        FirecrawlProvider::new(
            FirecrawlConfig::new(base, Some(secret("fc-secret")), timeout)
                .with_allow_private_targets(true),
        )
        .expect("firecrawl provider"),
    )
}

fn build_router(
    providers: Vec<Arc<dyn WebProvider>>,
    order: &[&str],
    research_order: &[&str],
    cache_ttl: Duration,
) -> WebRouter {
    WebRouter::new(
        providers,
        order.iter().map(|name| (*name).to_string()).collect(),
        research_order
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        cache_ttl,
    )
    .with_allow_private_targets(true)
}

fn read_success(url: &str, body: &str) -> serde_json::Value {
    json!({
        "results": [
            { "url": url, "title": "Read", "full_content": body, "excerpts": [] }
        ],
        "errors": []
    })
}

#[tokio::test]
async fn fallback_taxonomy_covers_auth_quota_timeout_404_and_422() {
    for (status, falls_back) in [
        (401_u16, true),
        (402, true),
        (403, true),
        (404, false),
        (408, true),
        (422, false),
        (400, false),
    ] {
        let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
            status,
            json!({ "error": "try another provider" }),
        )])
        .await;
        let (parallel_base, parallel_recorder) = spawn_stub([StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/ok", "title": "OK", "excerpts": ["ok"] }
                ]
            }),
        )])
        .await;
        let router = build_router(
            vec![
                tinyfish(&tiny_base, Duration::from_secs(5)),
                parallel(&parallel_base, Duration::from_secs(5)),
            ],
            &["tinyfish", "parallel"],
            &[],
            Duration::from_secs(0),
        );
        let result = router.search(SearchRequest::new("fallback matrix")).await;
        if falls_back {
            let results = result.expect("fallback provider succeeds");
            assert_eq!(results[0].provider, "parallel", "HTTP {status}");
            assert_eq!(parallel_recorder.count(), 1);
        } else {
            let error = result.expect_err("terminal status");
            assert_eq!(error.kind, ErrorKind::Client, "HTTP {status}");
            assert_eq!(error.status, Some(status), "HTTP {status}");
            assert_eq!(
                parallel_recorder.count(),
                0,
                "HTTP {status} must not fall back"
            );
        }
        assert_eq!(tiny_recorder.count(), 1);
    }
}

#[tokio::test]
async fn known_quota_body_is_fallback_eligible_even_on_403() {
    let (tiny_base, _) = spawn_stub([StubResponse::json(
        403,
        json!({ "error": { "code": "INSUFFICIENT_CREDITS", "message": "add funds" } }),
    )])
    .await;
    let (parallel_base, parallel_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [
                { "url": "https://example.com/ok", "title": "OK", "excerpts": ["ok"] }
            ]
        }),
    )])
    .await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let results = router
        .search(SearchRequest::new("quota"))
        .await
        .expect("quota fallback");
    assert_eq!(results[0].provider, "parallel");
    assert_eq!(parallel_recorder.count(), 1);
}

#[tokio::test]
async fn search_falls_back_on_429_and_5xx() {
    for status in [429_u16, 503] {
        let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
            status,
            json!({ "error": "try another provider" }),
        )])
        .await;
        let (parallel_base, parallel_recorder) = spawn_stub([StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/ok", "title": "OK", "excerpts": ["ok"] }
                ]
            }),
        )])
        .await;
        let router = build_router(
            vec![
                tinyfish(&tiny_base, Duration::from_secs(5)),
                parallel(&parallel_base, Duration::from_secs(5)),
            ],
            &["tinyfish", "parallel"],
            &[],
            Duration::from_secs(0),
        );
        let results = router
            .search(SearchRequest::new("fallback"))
            .await
            .expect("parallel succeeds");
        assert_eq!(results[0].provider, "parallel");
        assert_eq!(tiny_recorder.count(), 1);
        assert_eq!(parallel_recorder.count(), 1);
        assert_eq!(router.ledger().stats_for("tinyfish").failures, 1);
        assert_eq!(router.ledger().stats_for("parallel").successes, 1);
    }
}

#[tokio::test]
async fn search_falls_back_on_timeout_and_read_falls_back_on_5xx() {
    let (tiny_base, tiny_recorder) = spawn_stub([
        StubResponse::json(200, json!({ "results": [] })).delay(Duration::from_millis(400)),
        StubResponse::json(200, json!({ "results": [] })).delay(Duration::from_millis(400)),
    ])
    .await;
    let (parallel_base, parallel_recorder) = spawn_stub([
        StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/search", "title": "Search", "excerpts": ["s"] }
                ]
            }),
        ),
        StubResponse::json(200, read_success("https://example.com/read", "read body")),
    ])
    .await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_millis(50)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );

    let results = router
        .search(SearchRequest::new("timeout"))
        .await
        .expect("parallel search after timeout");
    assert_eq!(results[0].provider, "parallel");
    assert_eq!(router.ledger().stats_for("tinyfish").failures, 1);

    let read = router
        .read(ReadRequest::new("https://example.com/read"))
        .await
        .expect("parallel read after timeout");
    assert_eq!(read.provider, "parallel");
    assert_eq!(read.markdown, "read body");
    assert_eq!(parallel_recorder.count(), 2);
    assert_eq!(tiny_recorder.count(), 2);

    let (five_hundred_base, _) =
        spawn_stub([StubResponse::json(500, json!({ "error": "server broke" }))]).await;
    let (good_base, good_recorder) = spawn_stub([StubResponse::json(
        200,
        read_success("https://example.com/read", "fallback body"),
    )])
    .await;
    let read_router = build_router(
        vec![
            tinyfish(&five_hundred_base, Duration::from_secs(5)),
            parallel(&good_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let read = read_router
        .read(ReadRequest::new("https://example.com/read"))
        .await
        .expect("fallback read");
    assert_eq!(read.provider, "parallel");
    assert_eq!(read.markdown, "fallback body");
    assert_eq!(good_recorder.count(), 1);
}

#[tokio::test]
async fn per_url_read_failures_try_the_next_provider() {
    let target = "https://example.com/page";

    let (tiny_base, _) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [],
            "errors": [{ "url": target, "error": "bot blocked; fetch timed out" }]
        }),
    )])
    .await;
    let (parallel_base, parallel_recorder) = spawn_stub([StubResponse::json(
        200,
        read_success(target, "parallel body"),
    )])
    .await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let read = router
        .read(ReadRequest::new(target))
        .await
        .expect("tinyfish upstream error falls back");
    assert_eq!(read.provider, "parallel");
    assert_eq!(read.markdown, "parallel body");
    assert_eq!(parallel_recorder.count(), 1);

    let (parallel_error_base, _) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [],
            "errors": [{
                "url": target,
                "error_type": "fetch_error",
                "http_status_code": null,
                "content": null
            }]
        }),
    )])
    .await;
    let (firecrawl_base, firecrawl_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "success": true,
            "data": { "markdown": "firecrawl body", "metadata": { "statusCode": 200 } }
        }),
    )])
    .await;
    let router = build_router(
        vec![
            parallel(&parallel_error_base, Duration::from_secs(5)),
            firecrawl(&firecrawl_base, Duration::from_secs(5)),
        ],
        &["parallel", "firecrawl"],
        &[],
        Duration::from_secs(0),
    );
    let read = router
        .read(ReadRequest::new(target))
        .await
        .expect("parallel null-status error falls back");
    assert_eq!(read.provider, "firecrawl");
    assert_eq!(read.markdown, "firecrawl body");
    assert_eq!(firecrawl_recorder.count(), 1);

    let (firecrawl_empty_base, _) = spawn_stub([StubResponse::json(
        200,
        json!({ "success": true, "data": { "markdown": "", "metadata": { "statusCode": 200 } } }),
    )])
    .await;
    let (tiny_good_base, tiny_good_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [{
                "url": target,
                "final_url": target,
                "title": "Tiny",
                "text": "tiny body"
            }],
            "errors": []
        }),
    )])
    .await;
    let router = build_router(
        vec![
            firecrawl(&firecrawl_empty_base, Duration::from_secs(5)),
            tinyfish(&tiny_good_base, Duration::from_secs(5)),
        ],
        &["firecrawl", "tinyfish"],
        &[],
        Duration::from_secs(0),
    );
    let read = router
        .read(ReadRequest::new(target))
        .await
        .expect("firecrawl empty content falls back");
    assert_eq!(read.provider, "tinyfish");
    assert_eq!(read.markdown, "tiny body");
    assert_eq!(tiny_good_recorder.count(), 1);
}

#[tokio::test]
async fn search_does_not_fall_back_on_400() {
    let (tiny_base, tiny_recorder) =
        spawn_stub([StubResponse::json(400, json!({ "error": "bad query" }))]).await;
    let (parallel_base, parallel_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let error = router
        .search(SearchRequest::new("caller error"))
        .await
        .expect_err("400 is terminal");
    assert_eq!(error.kind, ErrorKind::Client);
    assert_eq!(error.status, Some(400));
    assert_eq!(tiny_recorder.count(), 1);
    assert_eq!(
        parallel_recorder.count(),
        0,
        "a 4xx must not try the next provider"
    );
}

#[tokio::test]
async fn invalid_query_url_and_input_caps_are_rejected_before_any_provider_call() {
    let (tiny_base, tiny_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let (parallel_base, parallel_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &["parallel", "tinyfish"],
        Duration::from_secs(60),
    );

    for request in [
        SearchRequest::new("   "),
        SearchRequest::new("x".repeat(2_001)),
        SearchRequest::new("ok").limit(99),
        SearchRequest::new("ok").queries((0..MAX_QUERIES + 1).map(|index| index.to_string())),
        SearchRequest::new("ok").include_domains((0..201).map(|index| format!("d{index}.example"))),
        SearchRequest::new("ok").objective("x".repeat(2_001)),
        SearchRequest::new("ok").recency_minutes(5_256_001),
    ] {
        let error = router.search(request).await.expect_err("bad search");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    for raw in [
        "ftp://example.com/file",
        "https://user:pass@example.com/file",
        &format!("https://example.com/{}", "a".repeat(2_100)),
    ] {
        let error = router
            .read(ReadRequest::new(raw))
            .await
            .expect_err("bad url");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
    }

    let research_error = router
        .research(ResearchRequest::new("  "))
        .await
        .expect_err("empty question");
    assert_eq!(research_error.kind, ErrorKind::InvalidInput);

    assert_eq!(tiny_recorder.count(), 0);
    assert_eq!(parallel_recorder.count(), 0);
    assert_eq!(router.ledger().entries().len(), 0);
}

#[tokio::test]
async fn private_and_loopback_targets_are_rejected_without_opt_in() {
    let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
        200,
        read_success("https://example.com", "body"),
    )])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(0),
    )
    .with_allow_private_targets(false);

    for raw in [
        "http://127.0.0.1/",
        "http://10.0.0.1/",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/",
        "http://localhost/",
    ] {
        let error = router
            .read(ReadRequest::new(raw))
            .await
            .expect_err("private target");
        assert_eq!(error.kind, ErrorKind::InvalidInput, "{raw}");
    }
    assert_eq!(tiny_recorder.count(), 0);
}

#[tokio::test]
async fn private_targets_work_with_explicit_opt_in() {
    let target = "http://127.0.0.1:9/private";
    let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [{
                "url": target,
                "final_url": target,
                "title": "Private",
                "text": "private body"
            }],
            "errors": []
        }),
    )])
    .await;
    let provider: Arc<dyn WebProvider> = Arc::new(
        TinyFishProvider::new(
            TinyFishConfig::new(secret("tf-secret"), Duration::from_secs(5))
                .with_search_url(format!("{tiny_base}/search"))
                .with_fetch_url(format!("{tiny_base}/fetch")),
        )
        .expect("provider defaults to no private targets"),
    );
    let router = build_router(vec![provider], &["tinyfish"], &[], Duration::from_secs(0));
    let read = router
        .read(ReadRequest::new(target))
        .await
        .expect("opt-in private read");
    assert_eq!(read.markdown, "private body");
    assert_eq!(tiny_recorder.requests()[0].body["urls"], json!([target]));
}

#[tokio::test]
async fn cache_hit_does_not_call_the_provider_twice() {
    let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [
                { "url": "https://example.com/cached", "title": "Cached", "snippet": "yes" }
            ]
        }),
    )])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(60),
    );
    let request = SearchRequest::new("same query");
    let first = router.search(request.clone()).await.expect("first");
    let second = router.search(request).await.expect("second");
    assert_eq!(first, second);
    assert_eq!(tiny_recorder.count(), 1);
    assert_eq!(router.cache_len(), 1);
    assert_eq!(router.ledger().stats_for("tinyfish").calls, 1);
}

#[tokio::test]
async fn cache_entries_expire_after_the_configured_ttl() {
    let (tiny_base, tiny_recorder) = spawn_stub([
        StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/one", "title": "One", "snippet": "one" }
                ]
            }),
        ),
        StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/two", "title": "Two", "snippet": "two" }
                ]
            }),
        ),
    ])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_millis(50),
    );
    let request = SearchRequest::new("expiring");
    let first = router.search(request.clone()).await.expect("first");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = router.search(request).await.expect("second");
    assert_eq!(first[0].title, "One");
    assert_eq!(second[0].title, "Two");
    assert_eq!(tiny_recorder.count(), 2);
}

#[tokio::test]
async fn identical_in_flight_searches_are_deduplicated() {
    let (tiny_base, tiny_recorder) = spawn_stub([StubResponse::json(
        200,
        json!({
            "results": [
                { "url": "https://example.com/one", "title": "One", "snippet": "one" }
            ]
        }),
    )
    .delay(Duration::from_millis(100))])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(60),
    );
    let request = SearchRequest::new("parallel callers");
    let (first, second) = tokio::join!(router.search(request.clone()), router.search(request));
    assert_eq!(first.expect("first").len(), 1);
    assert_eq!(second.expect("second").len(), 1);
    assert_eq!(
        tiny_recorder.count(),
        1,
        "single-flight should collapse the two calls"
    );
    assert_eq!(router.ledger().stats_for("tinyfish").calls, 1);
}

#[tokio::test]
async fn research_uses_the_configured_research_order() {
    let (parallel_base, parallel_recorder) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_live" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": {
                    "type": "text",
                    "content": "Research answer.",
                    "basis": [{ "citations": [{ "url": "https://example.com/source" }] }]
                }
            }),
        ),
    ])
    .await;
    let (tiny_base, tiny_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &["parallel", "tinyfish"],
        Duration::from_secs(0),
    );
    let result = router
        .research(ResearchRequest::new("what is the answer?"))
        .await
        .expect("parallel research");
    assert_eq!(result.answer, "Research answer.");
    assert_eq!(result.citations, ["https://example.com/source"]);
    assert_eq!(result.provider, "parallel");
    assert_eq!(parallel_recorder.count(), 2);
    assert_eq!(tiny_recorder.count(), 0);
}

#[tokio::test]
async fn unsupported_research_providers_are_skipped() {
    let (parallel_base, _) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_skip" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": { "type": "text", "content": "Parallel only.", "basis": [] }
            }),
        ),
    ])
    .await;
    let (tiny_base, tiny_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &["tinyfish", "parallel"],
        Duration::from_secs(0),
    );
    let result = router
        .research(ResearchRequest::new("skip unsupported"))
        .await
        .expect("parallel after unsupported tinyfish");
    assert_eq!(result.answer, "Parallel only.");
    assert_eq!(
        tiny_recorder.count(),
        0,
        "TinyFish research makes no HTTP call"
    );
    assert_eq!(
        router.ledger().stats_for("tinyfish").failures,
        1,
        "the unsupported attempt is still recorded"
    );
}

#[tokio::test]
async fn only_tinyfish_research_returns_no_providers() {
    let config = WebConfig::from_lookup(|name| match name {
        "DHI_WEB_PROVIDER" => Some("tinyfish".to_string()),
        "TINYFISH_API_KEY" => Some("tf-secret".to_string()),
        _ => None,
    })
    .expect("config");
    let router = WebRouter::from_config(&config).expect("router");
    assert!(router.research_provider_order().is_empty());
    let error = router
        .research(ResearchRequest::new("anything"))
        .await
        .expect_err("no research provider");
    assert_eq!(error.kind, ErrorKind::NoProviders);
    assert!(error.message().contains("PARALLEL_API_KEY"), "{error}");
    assert!(
        !error
            .message()
            .contains("TinyFish does not expose research"),
        "{error}"
    );
}

#[tokio::test]
async fn parallel_research_failure_is_returned_not_tinyfish_unsupported() {
    let (parallel_base, parallel_recorder) = spawn_stub([StubResponse::json(
        500,
        json!({ "error": "research backend down" }),
    )])
    .await;
    let (tiny_base, _) = spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let config = WebConfig::from_lookup(|name| match name {
        "TINYFISH_API_KEY" => Some("tf-secret".to_string()),
        "TINYFISH_SEARCH_URL" => Some(format!("{tiny_base}/search")),
        "TINYFISH_FETCH_URL" => Some(format!("{tiny_base}/fetch")),
        "PARALLEL_API_KEY" => Some("par-secret".to_string()),
        "PARALLEL_BASE_URL" => Some(parallel_base.clone()),
        _ => None,
    })
    .expect("config");
    let router = WebRouter::from_config(&config).expect("router");
    assert_eq!(router.research_provider_order(), ["parallel"]);
    let error = router
        .research(ResearchRequest::new("parallel fails"))
        .await
        .expect_err("parallel failure");
    assert_eq!(error.provider.as_deref(), Some("parallel"));
    assert_eq!(error.status, Some(500));
    assert!(!error.message().contains("TinyFish"), "{error}");
    assert_eq!(parallel_recorder.count(), 1);
}

#[tokio::test]
async fn all_failed_preserves_the_last_attempt_status_and_retry_after() {
    let (tiny_base, _) =
        spawn_stub([StubResponse::json(500, json!({ "error": "first failed" }))]).await;
    let (parallel_base, _) = spawn_stub([StubResponse::json(
        429,
        json!({ "error": "second failed" }),
    )
    .with_header("retry-after", "7")])
    .await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let error = router
        .search(SearchRequest::new("all fail"))
        .await
        .expect_err("all fail");
    assert_eq!(error.kind, ErrorKind::AllFailed);
    assert_eq!(error.provider.as_deref(), Some("parallel"));
    assert_eq!(error.status, Some(429));
    assert_eq!(error.retry_after, Some(Duration::from_secs(7)));
    assert_eq!(error.attempts().len(), 2);
}

#[tokio::test]
async fn all_provider_errors_are_redacted_in_the_router_message() {
    let (tiny_base, _) = spawn_stub([StubResponse::json(
        503,
        json!({ "error": "key tf-secret is over quota" }),
    )])
    .await;
    let (parallel_base, _) =
        spawn_stub([StubResponse::json(500, json!({ "error": "also failed" }))]).await;
    let router = build_router(
        vec![
            tinyfish(&tiny_base, Duration::from_secs(5)),
            parallel(&parallel_base, Duration::from_secs(5)),
        ],
        &["tinyfish", "parallel"],
        &[],
        Duration::from_secs(0),
    );
    let error = router
        .search(SearchRequest::new("redact"))
        .await
        .expect_err("all failed");
    assert_eq!(error.kind, ErrorKind::AllFailed);
    assert!(!error.message().contains("tf-secret"), "{error}");
    assert!(error.message().contains("[redacted]"), "{error}");
}

#[tokio::test]
async fn overall_search_deadline_cancels_the_chain() {
    let (tiny_base, _) = spawn_stub([
        StubResponse::json(200, json!({ "results": [] })).delay(Duration::from_millis(500))
    ])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(0),
    )
    .with_operation_timeout(Duration::from_millis(50));
    let error = router
        .search(SearchRequest::new("deadline"))
        .await
        .expect_err("deadline");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.provider.as_deref(), Some("router"));
}

#[tokio::test]
async fn tool_outputs_keep_stable_keys() {
    let (tiny_base, _) = spawn_stub([
        StubResponse::json(
            200,
            json!({
                "results": [
                    { "url": "https://example.com/result", "title": "Title", "snippet": "Snippet" }
                ]
            }),
        ),
        StubResponse::json(
            200,
            json!({
                "results": [{
                    "url": "https://example.com/read",
                    "final_url": "https://example.com/final",
                    "title": "Read",
                    "text": "# Body"
                }],
                "errors": []
            }),
        ),
    ])
    .await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(0),
    );
    let search = router
        .execute("web_search", &json!({ "query": "shape" }))
        .await
        .expect("search tool");
    assert_eq!(search["count"], 1);
    assert_eq!(search["results"][0]["title"], "Title");
    assert_eq!(search["results"][0]["url"], "https://example.com/result");
    assert_eq!(search["results"][0]["snippet"], "Snippet");
    assert_eq!(search["results"][0]["provider"], "tinyfish");

    let read = router
        .execute("web_read", &json!({ "url": "https://example.com/read" }))
        .await
        .expect("read tool");
    assert_eq!(read["url"], "https://example.com/final");
    assert_eq!(read["title"], "Read");
    assert_eq!(read["markdown"], "# Body");
    assert_eq!(read["truncated"], false);
    assert_eq!(read["provider"], "tinyfish");

    let (parallel_base, _) = spawn_stub([
        StubResponse::json(202, json!({ "run_id": "trun_shape" })),
        StubResponse::json(
            200,
            json!({
                "run": { "status": "completed" },
                "output": {
                    "type": "text",
                    "content": "Answer.",
                    "basis": [{ "citations": [{ "url": "https://example.com/source" }] }]
                }
            }),
        ),
    ])
    .await;
    let research_router = build_router(
        vec![parallel(&parallel_base, Duration::from_secs(5))],
        &["parallel"],
        &["parallel"],
        Duration::from_secs(0),
    );
    let research = research_router
        .execute("web_research", &json!({ "question": "shape?" }))
        .await
        .expect("research tool");
    assert_eq!(research["answer"], "Answer.");
    assert_eq!(research["citations"], json!(["https://example.com/source"]));
    assert_eq!(research["truncated"], false);
    assert_eq!(research["provider"], "parallel");
}

#[tokio::test]
async fn execute_rejects_unknown_tools_before_calling_a_provider() {
    let (tiny_base, tiny_recorder) =
        spawn_stub([StubResponse::json(200, json!({ "results": [] }))]).await;
    let router = build_router(
        vec![tinyfish(&tiny_base, Duration::from_secs(5))],
        &["tinyfish"],
        &[],
        Duration::from_secs(60),
    );
    let error = router
        .execute("web_parse", &json!({ "url": "https://example.com" }))
        .await
        .expect_err("web_parse is folded into web_read");
    assert_eq!(error.kind, ErrorKind::InvalidInput);
    assert_eq!(tiny_recorder.count(), 0);

    let output = router
        .execute("web_search", &json!({ "query": "tool path" }))
        .await
        .expect("tool execution");
    assert_eq!(output["count"], 0);
}
