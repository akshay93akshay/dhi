//! PostgreSQL tests for the tool control write model.
//!
//! Every test gets its own database through `#[sqlx::test]`; the shared
//! migration is applied from `dhi-store` and never edited here.

use dhi_tool_control::{
    PgToolControl, SkillUsageRecord, ToolControl, ToolControlAction, ToolControlError, ToolState,
};
use sqlx::PgPool;
use uuid::Uuid;

async fn seed_tool(pool: &PgPool, key: &str) {
    sqlx::query("INSERT INTO tool_catalog (key, source) VALUES ($1, 'builtin')")
        .bind(key)
        .execute(pool)
        .await
        .expect("seed tool catalog row");
}

fn state<'a>(states: &'a [ToolState], key: &str) -> &'a ToolState {
    states
        .iter()
        .find(|state| state.key == key)
        .unwrap_or_else(|| panic!("missing control state for {key:?}"))
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn set_enabled_toggles_and_rejects_unknown_keys(pool: PgPool) {
    seed_tool(&pool, "alpha").await;
    seed_tool(&pool, "beta").await;
    let control = PgToolControl::new(pool.clone());

    let states = control
        .apply(ToolControlAction::SetEnabled {
            key: "alpha".to_string(),
            enabled: false,
        })
        .await
        .expect("disable alpha");
    assert_eq!(states.len(), 2, "apply returns the full control state");
    assert!(!state(&states, "alpha").enabled);
    assert!(state(&states, "beta").enabled);
    let stored: bool = sqlx::query_scalar("SELECT enabled FROM tool_catalog WHERE key = 'alpha'")
        .fetch_one(&pool)
        .await
        .expect("read alpha");
    assert!(!stored, "the write must be committed before apply returns");

    let states = control
        .apply(ToolControlAction::SetEnabled {
            key: "alpha".to_string(),
            enabled: true,
        })
        .await
        .expect("enable alpha");
    assert!(state(&states, "alpha").enabled);
    let stored: bool = sqlx::query_scalar("SELECT enabled FROM tool_catalog WHERE key = 'alpha'")
        .fetch_one(&pool)
        .await
        .expect("read alpha");
    assert!(stored);

    for key in ["missing", ""] {
        let error = control
            .apply(ToolControlAction::SetEnabled {
                key: key.to_string(),
                enabled: true,
            })
            .await
            .expect_err("unknown keys must fail");
        assert_eq!(error, ToolControlError::UnknownTool(key.to_string()));
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tool_catalog")
        .fetch_one(&pool)
        .await
        .expect("count catalog");
    assert_eq!(count, 2, "an unknown key must not create a catalog row");
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn visibility_replacement_including_empty(pool: PgPool) {
    seed_tool(&pool, "alpha").await;
    let control = PgToolControl::new(pool.clone());

    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec![
                "worker".to_string(),
                "planner".to_string(),
                "worker".to_string(),
            ],
        })
        .await
        .expect("set visibility");
    let states = control.states().await.expect("states");
    assert_eq!(state(&states, "alpha").roles, ["planner", "worker"]);

    // Replacement removes roles that are not in the new set.
    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec!["reporter".to_string()],
        })
        .await
        .expect("replace visibility");
    let states = control.states().await.expect("states");
    assert_eq!(state(&states, "alpha").roles, ["reporter"]);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_visibility WHERE tool_key = 'alpha'")
            .fetch_one(&pool)
            .await
            .expect("count visibility");
    assert_eq!(count, 1);

    // An empty list is valid and means visible to nobody.
    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: Vec::new(),
        })
        .await
        .expect("clear visibility");
    let states = control.states().await.expect("states");
    assert!(state(&states, "alpha").roles.is_empty());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tool_visibility WHERE tool_key = 'alpha'")
            .fetch_one(&pool)
            .await
            .expect("count visibility");
    assert_eq!(count, 0);

    // Unknown keys fail closed and leave an existing set untouched.
    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec!["digest".to_string()],
        })
        .await
        .expect("set visibility");
    let error = control
        .apply(ToolControlAction::SetVisibility {
            key: "missing".to_string(),
            roles: vec!["worker".to_string()],
        })
        .await
        .expect_err("unknown key");
    assert_eq!(error, ToolControlError::UnknownTool("missing".to_string()));
    let states = control.states().await.expect("states");
    assert_eq!(state(&states, "alpha").roles, ["digest"]);
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn visibility_roles_are_validated_and_canonicalized(pool: PgPool) {
    seed_tool(&pool, "alpha").await;
    let control = PgToolControl::new(pool.clone());

    // Surrounding whitespace is ignored and specialist names are canonical.
    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec![
                " planner ".to_string(),
                "specialist: researcher ".to_string(),
            ],
        })
        .await
        .expect("canonical roles");
    let states = control.states().await.expect("states");
    assert_eq!(
        state(&states, "alpha").roles,
        ["planner", "specialist:researcher"]
    );

    for role in [
        "",
        "   ",
        "intern",
        "PLANNER",
        "specialist:",
        "specialist:   ",
        "planner\n",
        "worker\t",
    ] {
        let error = control
            .apply(ToolControlAction::SetVisibility {
                key: "alpha".to_string(),
                roles: vec!["worker".to_string(), role.to_string()],
            })
            .await
            .expect_err("invalid role");
        assert!(
            matches!(error, ToolControlError::Invalid(_)),
            "role {role:?} produced {error:?}"
        );
    }

    // Every rejected action left the previous set intact.
    let states = control.states().await.expect("states");
    assert_eq!(
        state(&states, "alpha").roles,
        ["planner", "specialist:researcher"]
    );
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn skill_usage_inserts_and_validates_optional_ids(pool: PgPool) {
    let control = PgToolControl::new(pool.clone());
    let task_id = Uuid::new_v4();
    let intent_id = Uuid::new_v4();

    control
        .record_skill_usage(&SkillUsageRecord {
            skill: " review ".to_string(),
            role: " specialist:Researcher ".to_string(),
            task_id: Some(task_id.to_string()),
            intent_id: Some(intent_id.to_string()),
            found: true,
        })
        .await
        .expect("record usage");
    let row: (String, String, Option<Uuid>, Option<Uuid>, bool) =
        sqlx::query_as("SELECT skill, role, task_id, intent_id, found FROM skill_usage")
            .fetch_one(&pool)
            .await
            .expect("read usage");
    assert_eq!(row.0, "review");
    assert_eq!(row.1, "specialist:Researcher");
    assert_eq!(row.2, Some(task_id));
    assert_eq!(row.3, Some(intent_id));
    assert!(row.4);

    // Task and intent are optional.
    control
        .record_skill_usage(&SkillUsageRecord {
            skill: "review".to_string(),
            role: "worker".to_string(),
            task_id: None,
            intent_id: None,
            found: false,
        })
        .await
        .expect("record usage without ids");
    let row: (Option<Uuid>, Option<Uuid>, bool) =
        sqlx::query_as("SELECT task_id, intent_id, found FROM skill_usage WHERE role = 'worker'")
            .fetch_one(&pool)
            .await
            .expect("read usage");
    assert_eq!(row, (None, None, false));

    let secret = "91e5f8d3-not-a-uuid-secret";
    let invalid = [
        SkillUsageRecord {
            skill: "review".to_string(),
            role: "worker".to_string(),
            task_id: Some(secret.to_string()),
            intent_id: None,
            found: true,
        },
        SkillUsageRecord {
            skill: "review".to_string(),
            role: "worker".to_string(),
            task_id: None,
            intent_id: Some(secret.to_string()),
            found: true,
        },
        SkillUsageRecord {
            skill: String::new(),
            role: "worker".to_string(),
            task_id: None,
            intent_id: None,
            found: true,
        },
        SkillUsageRecord {
            skill: "review".to_string(),
            role: "intern".to_string(),
            task_id: None,
            intent_id: None,
            found: true,
        },
        SkillUsageRecord {
            skill: "re\nview".to_string(),
            role: "worker".to_string(),
            task_id: None,
            intent_id: None,
            found: true,
        },
    ];
    for record in invalid {
        let error = control
            .record_skill_usage(&record)
            .await
            .expect_err("invalid usage record");
        assert!(
            matches!(error, ToolControlError::Invalid(_)),
            "unexpected error for {record:?}: {error:?}"
        );
        assert!(
            !error.to_string().contains(secret),
            "errors must not echo record values: {error}"
        );
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM skill_usage")
        .fetch_one(&pool)
        .await
        .expect("count usage");
    assert_eq!(count, 2, "rejected records must not be inserted");
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn states_are_key_ordered_and_roles_are_sorted(pool: PgPool) {
    for key in ["zulu", "alpha", "mike"] {
        seed_tool(&pool, key).await;
    }
    let control = PgToolControl::new(pool);

    control
        .apply(ToolControlAction::SetVisibility {
            key: "zulu".to_string(),
            roles: vec![
                "worker".to_string(),
                "digest".to_string(),
                "specialist:zeta".to_string(),
                "planner".to_string(),
            ],
        })
        .await
        .expect("set visibility");

    let states = control.states().await.expect("states");
    let keys: Vec<&str> = states.iter().map(|state| state.key.as_str()).collect();
    assert_eq!(keys, ["alpha", "mike", "zulu"]);
    assert_eq!(
        state(&states, "zulu").roles,
        ["digest", "planner", "specialist:zeta", "worker"]
    );
    assert!(state(&states, "alpha").roles.is_empty());
    assert!(state(&states, "mike").roles.is_empty());
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn a_failed_visibility_write_rolls_back_and_hides_server_details(pool: PgPool) {
    seed_tool(&pool, "alpha").await;
    let control = PgToolControl::new(pool.clone());
    control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec!["planner".to_string()],
        })
        .await
        .expect("seed visibility");

    sqlx::query(
        "CREATE FUNCTION dhi_reject_worker() RETURNS trigger AS $$
         BEGIN
             IF NEW.role = 'worker' THEN
                 RAISE EXCEPTION 'secret-token-from-database';
             END IF;
             RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&pool)
    .await
    .expect("create trigger function");
    sqlx::query(
        "CREATE TRIGGER dhi_reject_worker_insert
         BEFORE INSERT ON tool_visibility
         FOR EACH ROW EXECUTE FUNCTION dhi_reject_worker()",
    )
    .execute(&pool)
    .await
    .expect("create trigger");

    let error = control
        .apply(ToolControlAction::SetVisibility {
            key: "alpha".to_string(),
            roles: vec!["planner".to_string(), "worker".to_string()],
        })
        .await
        .expect_err("the trigger rejects worker");
    assert!(
        matches!(error, ToolControlError::Store(_)),
        "expected a store error, got {error:?}"
    );
    assert!(
        !error.to_string().contains("secret-token-from-database"),
        "raw database messages must not leak: {error}"
    );

    // The delete and the successful planner insert both rolled back, so the
    // original visibility set is intact.
    let roles: Vec<String> =
        sqlx::query_scalar("SELECT role FROM tool_visibility WHERE tool_key = 'alpha'")
            .fetch_all(&pool)
            .await
            .expect("read visibility");
    assert_eq!(roles, ["planner"]);
    let states = control.states().await.expect("states");
    assert_eq!(state(&states, "alpha").roles, ["planner"]);
}
