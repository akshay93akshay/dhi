//! Optional live smoke tests. They are ignored by default and skip cleanly
//! when the relevant key is absent. Keys are never printed.

use std::time::Duration;

use dhi_web::{
    ParallelConfig, ProviderMode, ReadRequest, SearchRequest, Secret, TinyFishConfig, WebConfig,
    WebRouter,
};
use serde_json::Value;

fn env_secret(name: &str) -> Option<Secret> {
    std::env::var(name).ok().and_then(Secret::new)
}

async fn top_hn_story(client: &reqwest::Client) -> (String, String) {
    let ids: Vec<u64> = client
        .get("https://hacker-news.firebaseio.com/v0/topstories.json")
        .send()
        .await
        .expect("HN top stories")
        .json()
        .await
        .expect("HN ids");
    for id in ids.into_iter().take(10) {
        let item: Value = client
            .get(format!(
                "https://hacker-news.firebaseio.com/v0/item/{id}.json"
            ))
            .send()
            .await
            .expect("HN item")
            .json()
            .await
            .expect("HN item json");
        let title = item.get("title").and_then(Value::as_str);
        let url = item.get("url").and_then(Value::as_str);
        if let (Some(title), Some(url)) = (title, url) {
            return (title.to_string(), url.to_string());
        }
    }
    panic!("no HN story with a URL in the top ten");
}

#[tokio::test]
#[ignore = "live network test; set TINYFISH_API_KEY to run"]
#[serial_test::serial]
async fn live_tinyfish_searches_today_top_hn_story_and_reads_it() {
    let Some(key) = env_secret("TINYFISH_API_KEY") else {
        eprintln!("skipping live TinyFish test: TINYFISH_API_KEY is not set");
        return;
    };
    let client = reqwest::Client::new();
    let (title, url) = top_hn_story(&client).await;

    let config = WebConfig {
        mode: ProviderMode::TinyFish,
        tinyfish: TinyFishConfig::new(key, Duration::from_secs(60)),
        ..WebConfig::default()
    };
    let router = WebRouter::from_config(&config).expect("TinyFish config");
    let results = router
        .search(SearchRequest::new(title.clone()).limit(5))
        .await
        .expect("live TinyFish search");
    assert!(
        !results.is_empty(),
        "TinyFish returned no results for {title:?}"
    );
    assert_eq!(results[0].provider, "tinyfish");

    let read = router
        .read(ReadRequest::new(url.clone()))
        .await
        .expect("live TinyFish read");
    assert_eq!(read.provider, "tinyfish");
    assert!(!read.markdown.trim().is_empty());
    eprintln!(
        "live TinyFish ok: {} result(s), read {} chars from {url}",
        results.len(),
        read.markdown.len()
    );
}

#[tokio::test]
#[ignore = "live network test; set PARALLEL_API_KEY to run"]
#[serial_test::serial]
async fn live_parallel_searches_today_top_hn_story_and_reads_it() {
    let Some(key) = env_secret("PARALLEL_API_KEY") else {
        eprintln!("skipping live Parallel test: PARALLEL_API_KEY is not set");
        return;
    };
    let client = reqwest::Client::new();
    let (title, url) = top_hn_story(&client).await;

    let config = WebConfig {
        mode: ProviderMode::Parallel,
        parallel: ParallelConfig::new(key, Duration::from_secs(60)),
        ..WebConfig::default()
    };
    let router = WebRouter::from_config(&config).expect("Parallel config");
    let results = router
        .search(SearchRequest::new(title.clone()).limit(5))
        .await
        .expect("live Parallel search");
    assert!(
        !results.is_empty(),
        "Parallel returned no results for {title:?}"
    );
    assert_eq!(results[0].provider, "parallel");

    let read = router
        .read(ReadRequest::new(url.clone()))
        .await
        .expect("live Parallel read");
    assert_eq!(read.provider, "parallel");
    assert!(!read.markdown.trim().is_empty());
    eprintln!(
        "live Parallel ok: {} result(s), read {} chars from {url}",
        results.len(),
        read.markdown.len()
    );
}
