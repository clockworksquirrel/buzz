use super::*;
use crate::managed_agents::retention::{
    get_pending_sync, get_retained_event, open_retention_db, retain_event,
    scoped_retention_db_path, tombstone_retention_d_tag,
};
use buzz_core_pkg::kind::{event_is_shared, KIND_TEAM};
use nostr::JsonUtil;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const KIND_DELETE: u32 = 5;

fn member(id: &str, display_name: &str) -> AgentDefinition {
    AgentDefinition {
        id: id.to_string(),
        display_name: display_name.to_string(),
        avatar_url: None,
        system_prompt: "Do the work.".to_string(),
        runtime: None,
        model: None,
        provider: None,
        name_pool: Vec::new(),
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

fn team() -> TeamRecord {
    TeamRecord {
        id: "team-abc".to_string(),
        name: "Catalog Team".to_string(),
        description: Some("A shared team".to_string()),
        instructions: None,
        persona_ids: vec!["m1".to_string(), "m2".to_string()],
        is_builtin: false,
        shared: false,
        catalog_source: None,
        source_dir: Some(PathBuf::from("/local/only/path")),
        is_symlink: false,
        symlink_target: None,
        version: None,
        created_at: "2026-07-30T00:00:00Z".to_string(),
        updated_at: "2026-07-30T00:00:00Z".to_string(),
    }
}

fn members() -> Vec<AgentDefinition> {
    vec![member("m1", "One"), member("m2", "Two")]
}

/// A retention database in its own scope directory, ready to write.
fn scoped_db(dir: &Path, relay_url: &str, owner: &str) -> PathBuf {
    let db_path = scoped_retention_db_path(dir, relay_url, owner);
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    db_path
}

fn retained_head(db_path: &Path, owner: &str) -> Option<RetainedEvent> {
    let conn = open_retention_db(db_path).unwrap();
    get_retained_event(&conn, KIND_TEAM_CATALOG, owner, "team-abc").unwrap()
}

// ── Publish / unshare ────────────────────────────────────────────────────────

#[test]
fn test_share_retains_a_pending_head_carrying_the_shared_tag() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);

    let (event, _, scoped_team) =
        prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();

    assert!(event_is_shared(&event));
    assert!(scoped_team.shared);
    let row = retained_head(&db_path, &owner).expect("the head is retained on share");
    assert!(row.pending_sync, "the flush loop must still owe a publish");
}

#[test]
fn test_unshare_publishes_a_newer_untagged_head_instead_of_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);

    let (shared_event, _, _) =
        prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();
    let (untagged_event, _, scoped_team) =
        prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(false)).unwrap();

    assert!(!event_is_shared(&untagged_event));
    assert!(!scoped_team.shared);
    assert!(
        untagged_event.created_at > shared_event.created_at,
        "the retraction must supersede the shared head monotonically"
    );
    let row = retained_head(&db_path, &owner).expect("unshare replaces the head, never deletes it");
    assert!(!retained_team_is_shared(Some(&row)));
    assert!(row.pending_sync);
}

#[test]
fn test_edit_without_an_override_preserves_the_scoped_share_state() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();

    let mut edited = team();
    edited.name = "Renamed Team".to_string();
    let (event, _, scoped_team) =
        prepare_team_publication_at(&db_path, &keys, &edited, &members(), None).unwrap();

    assert!(
        scoped_team.shared && event_is_shared(&event),
        "an ordinary edit must not silently unshare the team"
    );
}

#[test]
fn test_share_state_is_scoped_by_relay_and_owner() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let community_a = scoped_db(dir.path(), "wss://a.example", &owner);
    let community_b = scoped_db(dir.path(), "wss://b.example", &owner);

    prepare_team_publication_at(&community_a, &keys, &team(), &members(), Some(true)).unwrap();
    let (_, _, in_b) =
        prepare_team_publication_at(&community_b, &keys, &team(), &members(), None).unwrap();

    assert!(!in_b.shared, "one community's share choice must not leak");
    assert!(retained_team_is_shared(
        retained_head(&community_a, &owner).as_ref()
    ));
    assert!(!retained_team_is_shared(
        retained_head(&community_b, &owner).as_ref()
    ));
}

#[test]
fn test_oversized_team_fails_before_anything_is_enqueued() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    let mut huge = member("m1", "One");
    huge.system_prompt =
        "x".repeat(crate::managed_agents::team_catalog::MAX_SYSTEM_PROMPT_BYTES + 1);

    let error =
        prepare_team_publication_at(&db_path, &keys, &team(), &[huge], Some(true)).unwrap_err();

    assert!(
        error.contains("the system prompt for 'One'"),
        "the error must name the oversized field, got: {error}"
    );
    assert!(
        retained_head(&db_path, &owner).is_none(),
        "a projection the relay would refuse must never reach the pending queue"
    );
}

// ── Projection ───────────────────────────────────────────────────────────────

#[test]
fn test_resolvable_scope_projects_the_retained_share_state() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();
    let mut teams = vec![team()];

    project_scoped_team_sharing(
        Ok(RetentionScope {
            db_path,
            relay_url: "wss://a.example".to_string(),
            owner_keys: keys,
        }),
        &mut teams,
    );

    assert!(teams[0].shared);
}

#[test]
fn test_builtin_teams_project_as_unshared() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    // A head exists at the coordinate, so only the built-in guard can keep the
    // projection false.
    prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();
    let mut teams = vec![team()];
    teams[0].is_builtin = true;

    project_scoped_team_sharing(
        Ok(RetentionScope {
            db_path,
            relay_url: "wss://a.example".to_string(),
            owner_keys: keys,
        }),
        &mut teams,
    );

    assert!(!teams[0].shared, "built-in teams are never shareable");
}

#[test]
fn test_unresolvable_scope_projects_unshared_instead_of_failing() {
    let mut teams = vec![team()];
    teams[0].shared = true;
    // The real recovery-mode failure: `active_retention_scope` cannot resolve a
    // scope without signing keys, which is exactly what `identity_lost`
    // withholds.
    let state = crate::app_state::build_app_state();
    state
        .identity_lost
        .store(true, std::sync::atomic::Ordering::Release);
    let error = state
        .signing_keys()
        .expect_err("recovery mode must withhold signing keys");

    project_scoped_team_sharing(Err(error), &mut teams);

    assert!(
        !teams[0].shared,
        "an unresolvable scope degrades to unshared so list/create/update keep working"
    );
}

#[test]
fn test_unopenable_retention_db_projects_unshared_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let mut teams = vec![team()];
    teams[0].shared = true;

    project_scoped_team_sharing(
        Ok(RetentionScope {
            // A directory cannot be opened as the retention database.
            db_path: dir.path().to_path_buf(),
            relay_url: "wss://a.example".to_string(),
            owner_keys: nostr::Keys::generate(),
        }),
        &mut teams,
    );

    assert!(!teams[0].shared);
}

// ── Tombstone ────────────────────────────────────────────────────────────────

#[test]
fn test_delete_purges_the_catalog_head_and_enqueues_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    prepare_team_publication_at(&db_path, &keys, &team(), &members(), Some(true)).unwrap();

    tombstone_team_catalog_at(&db_path, &keys, "team-abc").unwrap();

    assert!(
        retained_head(&db_path, &owner).is_none(),
        "the purge must run first so an unpublished re-share cannot resurrect the entry"
    );
    let conn = open_retention_db(&db_path).unwrap();
    let pending = get_pending_sync(&conn).unwrap();
    let tombstone = pending
        .iter()
        .find(|row| row.kind == KIND_DELETE)
        .expect("the deletion is enqueued for the flush loop");
    assert_eq!(
        tombstone.d_tag,
        tombstone_retention_d_tag(KIND_TEAM_CATALOG, "team-abc")
    );
    assert!(tombstone.pending_sync, "an offline delete stays durable");
    let event = nostr::Event::from_json(&tombstone.raw_event).unwrap();
    assert!(
        event.tags.iter().any(|tag| tag.as_slice()
            == [
                "a".to_string(),
                format!("{KIND_TEAM_CATALOG}:{owner}:team-abc")
            ]),
        "the published deletion targets the 30178 coordinate"
    );
}

#[test]
fn test_catalog_tombstone_does_not_clobber_the_team_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    let owner = keys.public_key().to_hex();
    let db_path = scoped_db(dir.path(), "wss://a.example", &owner);
    let conn = open_retention_db(&db_path).unwrap();
    // The kind:30176 tombstone `delete_team` enqueues alongside this one. Both
    // carry kind 5 and the same team id, so only the folded-in target kind
    // keeps them on separate primary-key rows.
    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_DELETE,
            pubkey: owner.clone(),
            d_tag: tombstone_retention_d_tag(KIND_TEAM, "team-abc"),
            content: String::new(),
            created_at: 1,
            raw_event: "{}".to_string(),
            pending_sync: true,
        },
    )
    .unwrap();

    tombstone_team_catalog_at(&db_path, &keys, "team-abc").unwrap();

    let mut keys_seen: Vec<String> = get_pending_sync(&conn)
        .unwrap()
        .into_iter()
        .filter(|row| row.kind == KIND_DELETE)
        .map(|row| row.d_tag)
        .collect();
    keys_seen.sort();
    assert_eq!(keys_seen, ["30176:team-abc", "30178:team-abc"]);
}
