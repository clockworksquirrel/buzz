use super::*;
use crate::managed_agents::{
    retention::{get_retained_event, open_retention_db, retain_event, RetainedEvent},
    team_catalog::build_team_catalog_event,
    AgentDefinition, TeamRecord,
};
use buzz_core_pkg::kind::{event_is_shared, KIND_TEAM_CATALOG};
use nostr::JsonUtil;
use std::collections::BTreeMap;

const TEAM_ID: &str = "team-alpha";

fn member(id: &str, prompt: &str) -> AgentDefinition {
    AgentDefinition {
        id: id.to_string(),
        display_name: id.to_string(),
        avatar_url: None,
        system_prompt: prompt.to_string(),
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
        id: TEAM_ID.to_string(),
        name: "Alpha".to_string(),
        description: None,
        instructions: None,
        persona_ids: vec!["m1".to_string()],
        is_builtin: false,
        shared: false,
        catalog_source: None,
        source_dir: None,
        is_symlink: false,
        symlink_target: None,
        version: None,
        created_at: "2026-07-30T00:00:00Z".to_string(),
        updated_at: "2026-07-30T00:00:00Z".to_string(),
    }
}

fn write_stores(base_dir: &Path, teams: &[TeamRecord], personas: &[AgentDefinition]) {
    std::fs::write(
        base_dir.join("teams.json"),
        serde_json::to_string(teams).unwrap(),
    )
    .unwrap();
    std::fs::write(
        base_dir.join("personas.json"),
        serde_json::to_string(personas).unwrap(),
    )
    .unwrap();
}

/// Retain a catalog head for `team`/`members`, as the share toggle would.
fn retain_head(
    base_dir: &Path,
    keys: &nostr::Keys,
    team: &TeamRecord,
    members: &[AgentDefinition],
) {
    let event = build_team_catalog_event(team, members, true)
        .unwrap()
        .sign_with_keys(keys)
        .unwrap();
    let conn = open_retention_db(&base_dir.join("retention.db")).unwrap();
    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_TEAM_CATALOG,
            pubkey: keys.public_key().to_hex(),
            d_tag: team.id.clone(),
            content: event.content.to_string(),
            created_at: event.created_at.as_secs() as i64,
            raw_event: event.as_json(),
            pending_sync: false,
        },
    )
    .unwrap();
}

fn head(base_dir: &Path, keys: &nostr::Keys) -> Option<RetainedEvent> {
    let conn = open_retention_db(&base_dir.join("retention.db")).unwrap();
    get_retained_event(
        &conn,
        KIND_TEAM_CATALOG,
        &keys.public_key().to_hex(),
        TEAM_ID,
    )
    .unwrap()
}

fn reconcile(base_dir: &Path, keys: &nostr::Keys) -> Result<u32, String> {
    reconcile_team_catalog_heads_at(base_dir, keys, &base_dir.join("retention.db"))
}

fn head_is_shared(row: &RetainedEvent) -> bool {
    event_is_shared(&nostr::Event::from_json(&row.raw_event).unwrap())
}

#[test]
fn test_member_edit_republishes_a_newer_shared_head() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    let before = head(base.path(), &keys).unwrap();
    // The team is untouched; only the member's prompt changed, which the
    // publish path never observes.
    write_stores(base.path(), &[team()], &[member("m1", "Rewritten.")]);

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 1);

    let after = head(base.path(), &keys).unwrap();
    assert!(after.content.contains("Rewritten."));
    assert!(head_is_shared(&after), "a refresh stays discoverable");
    assert!(
        after.pending_sync,
        "the refreshed head is queued to publish"
    );
    assert!(after.created_at > before.created_at);
}

#[test]
fn test_unchanged_team_is_left_alone() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    write_stores(base.path(), &[team()], &[member("m1", "Original.")]);

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 0);

    assert!(
        !head(base.path(), &keys).unwrap().pending_sync,
        "an unchanged team must not churn pending_sync on every boot"
    );
}

#[test]
fn test_deleted_member_retracts_the_head_without_deleting_it() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    let before = head(base.path(), &keys).unwrap();
    // The member is gone, so the team can no longer be projected at all.
    write_stores(base.path(), &[team()], &[]);

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 1);

    let after = head(base.path(), &keys).unwrap();
    assert!(
        !head_is_shared(&after),
        "a team that can no longer be projected must stop being discoverable"
    );
    assert_eq!(
        after.content, before.content,
        "retraction replays the retained body so the owner can re-share it"
    );
    assert!(after.pending_sync);
    assert!(after.created_at > before.created_at);
}

#[test]
fn test_retraction_is_not_repeated_on_the_next_boot() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    write_stores(base.path(), &[team()], &[]);
    reconcile(base.path(), &keys).unwrap();

    assert_eq!(
        reconcile(base.path(), &keys).unwrap(),
        0,
        "the head is already untagged, so there is nothing left to retract"
    );
}

#[test]
fn test_unshared_head_is_never_touched() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    // An unshared head with a member that no longer exists — the retraction
    // trigger — must still be left alone: it is not discoverable.
    let event = build_team_catalog_event(&team(), &[member("m1", "Original.")], false)
        .unwrap()
        .sign_with_keys(&keys)
        .unwrap();
    let conn = open_retention_db(&base.path().join("retention.db")).unwrap();
    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_TEAM_CATALOG,
            pubkey: keys.public_key().to_hex(),
            d_tag: TEAM_ID.to_string(),
            content: event.content.to_string(),
            created_at: event.created_at.as_secs() as i64,
            raw_event: event.as_json(),
            pending_sync: false,
        },
    )
    .unwrap();
    write_stores(base.path(), &[team()], &[]);

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 0);

    assert!(!head(base.path(), &keys).unwrap().pending_sync);
}

#[test]
fn test_team_with_no_head_is_skipped() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    write_stores(base.path(), &[team()], &[member("m1", "Original.")]);

    assert_eq!(
        reconcile(base.path(), &keys).unwrap(),
        0,
        "a team the owner never shared must not be published by a boot reconcile"
    );
    assert!(head(base.path(), &keys).is_none());
}

#[test]
fn test_members_are_read_from_the_unified_agent_store_after_the_fold() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    // Post-fold there is no personas.json; definitions are key-less records in
    // managed-agents.json. Reading only personas.json would see zero members
    // and retract every shared team on the next boot.
    std::fs::write(
        base.path().join("teams.json"),
        serde_json::to_string(&[team()]).unwrap(),
    )
    .unwrap();
    let folded: Vec<crate::managed_agents::ManagedAgentRecord> =
        vec![member("m1", "Original.").into_agent_record()];
    std::fs::write(
        base.path().join("managed-agents.json"),
        serde_json::to_string(&folded).unwrap(),
    )
    .unwrap();

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 0);

    let after = head(base.path(), &keys).unwrap();
    assert!(head_is_shared(&after), "the team must not be retracted");
    assert!(!after.pending_sync);
}

#[test]
fn test_builtin_teams_are_skipped() {
    let base = tempfile::tempdir().unwrap();
    let keys = nostr::Keys::generate();
    retain_head(base.path(), &keys, &team(), &[member("m1", "Original.")]);
    let mut builtin = team();
    builtin.is_builtin = true;
    write_stores(base.path(), &[builtin], &[]);

    assert_eq!(reconcile(base.path(), &keys).unwrap(), 0);
}
