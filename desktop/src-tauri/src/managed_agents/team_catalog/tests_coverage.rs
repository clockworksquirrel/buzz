//! Additional coverage tests for `team_catalog` — split from `tests.rs` to
//! stay under the file-size gate. Included via `#[path]` from `team_catalog.rs`.

use super::*;
use std::collections::BTreeMap;

fn member(id: &str, display_name: &str) -> AgentDefinition {
    AgentDefinition {
        id: id.to_string(),
        display_name: display_name.to_string(),
        avatar_url: None,
        system_prompt: "Do the work.".to_string(),
        runtime: Some("goose".to_string()),
        model: Some("claude-opus-4".to_string()),
        provider: Some("anthropic".to_string()),
        name_pool: vec!["Alpha".to_string()],
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        team_catalog_source: None,
        env_vars: BTreeMap::new(),
        respond_to: None,
        respond_to_allowlist: Vec::new(),
        parallelism: None,
        created_at: "2026-07-30T00:00:00Z".to_string(),
        updated_at: "2026-07-30T00:00:00Z".to_string(),
    }
}

fn team() -> crate::managed_agents::TeamRecord {
    crate::managed_agents::TeamRecord {
        id: "team-abc".to_string(),
        name: "Catalog Team".to_string(),
        description: Some("A shared team".to_string()),
        instructions: None,
        persona_ids: vec!["m1".to_string(), "m2".to_string()],
        is_builtin: false,
        shared: false,
        catalog_source: None,
        source_dir: Some(std::path::PathBuf::from("/local/only/path")),
        is_symlink: false,
        symlink_target: None,
        version: None,
        created_at: "2026-07-30T00:00:00Z".to_string(),
        updated_at: "2026-07-30T00:00:00Z".to_string(),
    }
}

// ── Untouched built-in projection ─────────────────────────────────────────

#[test]
fn test_real_builtin_without_avatar_mutation_projects_successfully() {
    // Retrieve the actual built-in record that `ensure_built_in_personas`
    // installs — no `avatar_url` mutation. The oversized avatar is stripped
    // inside `member_projection` (built-in path). If the logic is correct
    // the build succeeds and the projected member has no avatar field.
    let builtin =
        crate::managed_agents::built_in_persona_definition("builtin:fizz", "2026-07-30T00:00:00Z")
            .expect("builtin:fizz must exist");
    let has_large_avatar = builtin
        .avatar_url
        .as_deref()
        .is_some_and(|url| url.len() > MAX_AVATAR_URL_BYTES);
    let content = build_team_catalog_content(&team(), &[builtin]).expect(
        "a team containing a real built-in must project successfully without avatar mutation",
    );
    assert_eq!(content.members.len(), 1);
    if has_large_avatar {
        assert!(
            content.members[0].avatar_url.is_none(),
            "oversized built-in avatar must be omitted, not rejected"
        );
    }
    assert!(
        validate_team_catalog_content(&content).is_ok(),
        "projected content must pass full validation"
    );
}

// ── Tombstone transaction: rollback on INSERT failure ──────────────────────

#[test]
fn test_tombstone_transaction_rolls_back_delete_when_insert_fails() {
    // Use a `BEFORE INSERT` trigger to force the INSERT step to fail,
    // and verify the DELETE is rolled back (head still present after error).
    use crate::managed_agents::retention::{
        get_retained_event, open_retention_db, retain_event, scoped_retention_db_path,
        RetainedEvent,
    };
    use buzz_core_pkg::kind::KIND_TEAM_CATALOG;
    use nostr::JsonUtil;

    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_retention_db_path(dir.path(), "wss://a.example", &owner);
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let t = team();
    let m = member("m1", "Sentinel.");
    let head_event = build_team_catalog_event(&t, &[m], true)
        .unwrap()
        .sign_with_keys(&keys)
        .unwrap();
    let conn = open_retention_db(&db_path).unwrap();
    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_TEAM_CATALOG,
            pubkey: owner.clone(),
            d_tag: "team-abc".to_string(),
            content: head_event.content.to_string(),
            created_at: head_event.created_at.as_secs() as i64,
            raw_event: head_event.as_json(),
            pending_sync: false,
        },
    )
    .unwrap();

    conn.execute_batch(
        "CREATE TRIGGER block_all_inserts BEFORE INSERT ON persona_events
         BEGIN
             SELECT RAISE(ABORT, 'insert blocked by test trigger');
         END;",
    )
    .unwrap();
    drop(conn);

    let result = tombstone_team_catalog_coordinate(&db_path, &keys, "team-abc");
    assert!(result.is_err(), "tombstone with INSERT trigger must fail");
    let err = result.unwrap_err();
    assert!(
        err.contains("insert blocked by test trigger") || err.contains("blocked"),
        "error must name the trigger cause; got: {err}"
    );

    let conn = open_retention_db(&db_path).unwrap();
    let head = get_retained_event(&conn, KIND_TEAM_CATALOG, &owner, "team-abc").unwrap();
    assert!(
        head.is_some(),
        "DELETE must be rolled back when INSERT fails so the head is not lost"
    );
}
