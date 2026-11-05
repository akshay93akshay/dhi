//! Web provider settings store tests.
//!
//! Keys are write-only: list reads expose only the saved hint, while the
//! client-construction read (with keys) is used by the router builder.

use dhi_store::Store;
use dhi_store::mgmt::WebProviderSettingUpsert;
use sqlx::PgPool;

fn setting(
    provider: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
    timeout_secs: Option<u64>,
    enabled: Option<bool>,
) -> WebProviderSettingUpsert {
    WebProviderSettingUpsert {
        provider: provider.to_string(),
        api_key: api_key.map(str::to_string),
        base_url: base_url.map(str::to_string),
        timeout_secs,
        enabled,
    }
}

#[sqlx::test]
async fn insert_list_update_and_clear_round_trip(pool: PgPool) {
    let store = Store::new(pool);
    assert!(store.list_web_provider_settings().await.unwrap().is_empty());
    assert!(
        store
            .list_web_provider_settings_with_keys()
            .await
            .unwrap()
            .is_empty()
    );

    let row = store
        .upsert_web_provider_setting(&setting(
            "tinyfish",
            Some("tf-live-1234"),
            Some("http://127.0.0.1:9999"),
            Some(30),
            Some(true),
        ))
        .await
        .expect("insert");
    assert_eq!(row.provider, "tinyfish");
    assert_eq!(row.api_key_hint.as_deref(), Some("…1234"));
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9999"));
    assert_eq!(row.timeout_secs, Some(30));
    assert!(row.enabled);
    assert_eq!(row.priority, 1);

    // The key never appears in the outward row or its JSON.
    let json = serde_json::to_string(&row).expect("serialize");
    assert!(
        !json.contains("tf-live-1234"),
        "serialized output leaked a raw key"
    );
    assert!(!json.contains("api_key\""), "{json}");
    let full = store
        .list_web_provider_settings_with_keys()
        .await
        .expect("list full");
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].api_key.as_deref(), Some("tf-live-1234"));
    assert!(!format!("{:?}", full[0]).contains("tf-live-1234"));

    // An absent key keeps the stored key; an empty key does too.
    let row = store
        .upsert_web_provider_setting(&setting("tinyfish", None, None, Some(45), Some(false)))
        .await
        .expect("partial update");
    assert_eq!(row.api_key_hint.as_deref(), Some("…1234"));
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9999"));
    assert_eq!(row.timeout_secs, Some(45));
    assert!(!row.enabled);

    let row = store
        .upsert_web_provider_setting(&setting("tinyfish", Some(""), None, None, None))
        .await
        .expect("empty key keeps");
    assert_eq!(row.api_key_hint.as_deref(), Some("…1234"));

    // Clearing keeps the row and every non-key setting.
    let row = store
        .clear_web_provider_key("tinyfish")
        .await
        .expect("clear");
    assert_eq!(row.api_key_hint, None);
    assert_eq!(row.base_url.as_deref(), Some("http://127.0.0.1:9999"));
    assert_eq!(row.timeout_secs, Some(45));
    assert!(!row.enabled);
    let full = store
        .list_web_provider_settings_with_keys()
        .await
        .expect("list full after clear");
    assert_eq!(full.len(), 1, "the row is kept");
    assert_eq!(full[0].api_key, None);
    assert_eq!(full[0].api_key_hint, None);

    // Clearing an absent row creates it with a null key, so an environment
    // key cannot silently return through the absence of a row.
    let row = store
        .clear_web_provider_key("parallel")
        .await
        .expect("create cleared row");
    assert_eq!(row.provider, "parallel");
    assert_eq!(row.api_key_hint, None);
    assert_eq!(row.priority, 2);
    assert!(row.enabled);
    assert_eq!(store.list_web_provider_settings().await.unwrap().len(), 2);
}

#[sqlx::test]
async fn empty_base_url_clears_the_override_and_order_is_documented(pool: PgPool) {
    let store = Store::new(pool);
    store
        .upsert_web_provider_setting(&setting(
            "firecrawl",
            Some("fc-live-9876"),
            Some("http://127.0.0.1:3002"),
            Some(15),
            None,
        ))
        .await
        .expect("insert firecrawl");
    store
        .upsert_web_provider_setting(&setting(
            "parallel",
            Some("par-live-5555"),
            None,
            None,
            None,
        ))
        .await
        .expect("insert parallel");
    store
        .upsert_web_provider_setting(&setting("tinyfish", Some("tf-live-4444"), None, None, None))
        .await
        .expect("insert tinyfish");

    let rows = store.list_web_provider_settings().await.expect("list");
    assert_eq!(
        rows.iter()
            .map(|row| row.provider.as_str())
            .collect::<Vec<_>>(),
        ["tinyfish", "parallel", "firecrawl"]
    );
    assert_eq!(
        rows.iter().map(|row| row.priority).collect::<Vec<_>>(),
        [1, 2, 3]
    );

    let row = store
        .upsert_web_provider_setting(&setting("firecrawl", None, Some(""), None, None))
        .await
        .expect("clear base URL");
    assert_eq!(row.base_url, None);
    assert_eq!(row.api_key_hint.as_deref(), Some("…9876"));
    assert_eq!(row.timeout_secs, Some(15));
}

#[sqlx::test]
async fn invalid_provider_and_timeout_are_rejected(pool: PgPool) {
    let store = Store::new(pool);
    assert!(
        store
            .upsert_web_provider_setting(&setting("bing", Some("x"), None, None, None))
            .await
            .is_err()
    );
    assert!(
        store
            .upsert_web_provider_setting(&setting("tinyfish", None, None, Some(0), None))
            .await
            .is_err()
    );
    assert!(
        store
            .upsert_web_provider_setting(&setting("tinyfish", None, None, Some(3_601), None))
            .await
            .is_err()
    );
    assert!(store.clear_web_provider_key("bing").await.is_err());
}
