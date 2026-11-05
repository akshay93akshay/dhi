use std::collections::HashMap;
use std::time::Duration;

use dhi_web::{ErrorKind, ProviderKind, ProviderMode, SearchRequest, WebConfig, WebRouter};

fn config(values: &[(&str, &str)]) -> WebConfig {
    let values = values
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect::<HashMap<_, _>>();
    WebConfig::from_lookup(move |name| values.get(name).cloned()).expect("valid config")
}

#[test]
fn default_order_skips_missing_keys_with_clear_reasons() {
    let config = config(&[]);
    assert_eq!(config.mode, ProviderMode::Auto);
    assert_eq!(
        config.search_read_order(),
        &[
            ProviderKind::TinyFish,
            ProviderKind::Parallel,
            ProviderKind::Firecrawl
        ]
    );
    assert_eq!(config.research_order(), &[ProviderKind::Parallel]);

    let router = WebRouter::from_config(&config).expect("router");
    assert!(router.provider_order().is_empty());
    assert_eq!(router.skipped_providers().len(), 3);
    for skip in router.skipped_providers() {
        assert!(
            skip.reason.contains("TINYFISH_API_KEY")
                || skip.reason.contains("PARALLEL_API_KEY")
                || skip.reason.contains("FIRECRAWL"),
            "unclear skip reason: {}",
            skip.reason
        );
    }
}

#[test]
fn all_keys_enable_the_default_order_and_firecrawl_local_needs_no_key() {
    let config = config(&[
        ("TINYFISH_API_KEY", "tf-secret"),
        ("PARALLEL_API_KEY", "par-secret"),
        ("FIRECRAWL_BASE_URL", "http://127.0.0.1:3002"),
    ]);
    let router = WebRouter::from_config(&config).expect("router");
    assert_eq!(
        router.provider_order(),
        ["tinyfish", "parallel", "firecrawl"]
    );
    assert_eq!(router.research_provider_order(), ["parallel"]);
    assert!(router.skipped_providers().is_empty());
    assert_eq!(router.cache_ttl(), Duration::from_secs(300));
}

#[test]
fn forced_firecrawl_has_no_research_order() {
    let config = config(&[
        ("DHI_WEB_PROVIDER", "firecrawl"),
        ("FIRECRAWL_BASE_URL", "http://localhost:3002"),
    ]);
    let router = WebRouter::from_config(&config).expect("router");
    assert_eq!(router.provider_order(), ["firecrawl"]);
    assert!(router.research_provider_order().is_empty());

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let error = runtime
        .block_on(router.research(dhi_web::ResearchRequest::new("anything")))
        .expect_err("Firecrawl cannot research");
    assert_eq!(error.kind, ErrorKind::NoProviders);
    assert!(error.message().contains("web_research"), "{error}");
}

#[test]
fn no_provider_error_names_the_missing_key() {
    let config = config(&[]);
    let router = WebRouter::from_config(&config).expect("router");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let error = runtime
        .block_on(router.search(SearchRequest::new("anything")))
        .expect_err("no providers");
    assert_eq!(error.kind, ErrorKind::NoProviders);
    assert!(error.message().contains("TINYFISH_API_KEY"), "{error}");
    assert!(error.message().contains("PARALLEL_API_KEY"), "{error}");
}
