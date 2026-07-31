//! Additional coverage tests for `adopt` — split from `tests.rs` to stay
//! under the file-size gate.
//!
//! Included via `#[cfg(test)] #[path = "adopt/tests_coverage.rs"] mod tests_coverage;`
//! in `adopt.rs`. All test helpers from `tests.rs` are NOT available here;
//! each test imports what it needs directly.

fn plain_team(persona_ids: Vec<String>) -> crate::managed_agents::TeamRecord {
    crate::managed_agents::TeamRecord {
        id: "team-alpha".to_string(),
        name: "Alpha".to_string(),
        description: None,
        instructions: None,
        persona_ids,
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

fn plain_source(owner_pubkey: &str) -> crate::managed_agents::TeamCatalogSource {
    crate::managed_agents::TeamCatalogSource {
        owner_pubkey: owner_pubkey.to_string(),
        team_d_tag: "team-alpha".to_string(),
    }
}

// ── Untouched built-in end-to-end: publish + plan_add without avatar mutation

#[test]
fn test_untouched_real_builtin_round_trips_through_publish_and_plan_add() {
    // End-to-end path for the built-in reuse fix: a real built-in persona
    // (with its full ~170 KiB avatar_url) is projected by the publisher,
    // then received and re-matched by the recipient — all without any
    // avatar mutation. The avatar is stripped inside `member_projection`
    // (built-in path only), so both publisher and recipient compute identical
    // hashes, and `plan_add` reuses the local built-in instead of copying.
    use super::apply::plan_add;
    use crate::managed_agents::team_catalog::{
        build_team_catalog_event, team_catalog_content_from_event, MAX_AVATAR_URL_BYTES,
    };

    let local =
        crate::managed_agents::built_in_persona_definition("builtin:fizz", "2026-07-30T00:00:00Z")
            .expect("builtin:fizz must exist");
    let has_large_avatar = local
        .avatar_url
        .as_deref()
        .is_some_and(|url| url.len() > MAX_AVATAR_URL_BYTES);

    let team = plain_team(vec![local.id.clone()]);
    let keys = nostr::Keys::generate();
    let event = build_team_catalog_event(&team, std::slice::from_ref(&local), true)
        .expect("real built-in must project without avatar mutation")
        .sign_with_keys(&keys)
        .unwrap();
    let source = crate::managed_agents::TeamCatalogSource {
        owner_pubkey: keys.public_key().to_hex(),
        team_d_tag: team.id.clone(),
    };

    let published_content =
        team_catalog_content_from_event(&event).expect("projected event must be valid");

    assert_eq!(published_content.members.len(), 1);
    let pm = &published_content.members[0];

    if has_large_avatar {
        assert!(
            pm.avatar_url.is_none(),
            "oversized built-in avatar must be stripped from the published projection"
        );
    }
    assert_eq!(
        pm.builtin_slug.as_deref(),
        Some("fizz"),
        "built-in reuse hint must be present"
    );
    assert!(
        pm.projection_hash.is_some(),
        "projection hash must be present for reuse"
    );

    // Recipient side: plan_add with the local built-in present.
    let plan_result = plan_add(
        std::slice::from_ref(&local),
        &[],
        &source,
        &published_content,
        "2026-07-30T00:00:00Z",
    )
    .expect("add with matching local built-in must succeed");
    let (after_personas, after_teams) = plan_result.stores.expect("add must write stores");

    assert_eq!(
        after_personas.len(),
        1,
        "local built-in is reused, no new copy minted"
    );
    assert_eq!(
        after_personas[0].id, local.id,
        "reused record has the local built-in's id"
    );
    assert_eq!(after_teams.len(), 1);
}

// ── Allowlist adoption: adopted persona is launch-valid

#[test]
fn test_allowlist_respond_to_is_normalized_to_owner_only_on_adoption() {
    use super::apply::plan_add;
    use crate::managed_agents::team_catalog::{
        TeamCatalogContent, TeamCatalogMember, TEAM_CATALOG_SCHEMA_VERSION,
    };

    let owner = "e".repeat(64);
    let source = plain_source(&owner);
    let published = TeamCatalogMember {
        member_key: "m1".to_string(),
        display_name: "Review Agent".to_string(),
        system_prompt: Some("Review the work.".to_string()),
        avatar_url: None,
        runtime: None,
        model: None,
        provider: None,
        name_pool: Vec::new(),
        respond_to: Some("allowlist".to_string()),
        parallelism: None,
        builtin_slug: None,
        projection_hash: None,
    };
    let body = TeamCatalogContent {
        v: TEAM_CATALOG_SCHEMA_VERSION,
        name: "Reviewers".to_string(),
        description: None,
        instructions: None,
        members: vec![published],
    };

    let plan_result =
        plan_add(&[], &[], &source, &body, "2026-07-30T00:00:00Z").expect("add must succeed");
    let (personas, _) = plan_result.stores.expect("add must write stores");

    let copy = &personas[0];
    assert_eq!(
        copy.respond_to.as_deref(),
        Some("owner-only"),
        "allowlist must be normalized to owner-only at adoption"
    );
    assert!(
        copy.respond_to_allowlist.is_empty(),
        "adopted copy must have no allowlist entries"
    );

    // Verify the persona is launch-valid.
    let mint_result = crate::managed_agents::resolve_mint_behavioral_defaults(
        copy.respond_to
            .as_deref()
            .and_then(|w| crate::managed_agents::RespondTo::parse_wire(w).ok()),
        copy.respond_to_allowlist.clone(),
        None,
        None,
    );
    assert!(
        mint_result.is_ok(),
        "adopted persona with normalized respond_to must be launch-valid: {mint_result:?}"
    );
}

// ── delete_catalog_team_at seam: production-path delete → persist → re-add

#[test]
fn test_delete_catalog_team_seam_then_re_add_reactivates_copies() {
    use super::apply::plan_add;
    use crate::managed_agents::team_catalog::{
        TeamCatalogContent, TeamCatalogMember, TEAM_CATALOG_SCHEMA_VERSION,
    };

    let dir = tempfile::tempdir().unwrap();
    let owner = "f".repeat(64);
    let d_tag = "team-delta";

    let copy_member = TeamCatalogMember {
        member_key: "mk1".to_string(),
        display_name: "Work Agent".to_string(),
        system_prompt: Some("Do it.".to_string()),
        avatar_url: None,
        runtime: None,
        model: None,
        provider: None,
        name_pool: Vec::new(),
        respond_to: None,
        parallelism: None,
        builtin_slug: None,
        projection_hash: None,
    };
    let body = TeamCatalogContent {
        v: TEAM_CATALOG_SCHEMA_VERSION,
        name: "Delta".to_string(),
        description: None,
        instructions: None,
        members: vec![copy_member],
    };
    let src = crate::managed_agents::TeamCatalogSource {
        owner_pubkey: owner.clone(),
        team_d_tag: d_tag.to_string(),
    };

    // Step 1: plan and persist the initial add.
    let initial =
        plan_add(&[], &[], &src, &body, "2026-07-30T00:00:00Z").expect("first add must succeed");
    let (personas, teams) = initial.stores.expect("first add must write stores");
    let copy_id = personas[0].id.clone();

    let personas_path = dir.path().join("personas.json");
    let teams_path = dir.path().join("teams.json");
    std::fs::write(&personas_path, serde_json::to_string(&personas).unwrap()).unwrap();
    std::fs::write(&teams_path, serde_json::to_string(&teams).unwrap()).unwrap();

    // Step 2: delete through the production seam.
    crate::managed_agents::delete_catalog_team_at(&personas_path, &teams_path, &teams[0].id)
        .expect("delete must succeed");

    // Reload from disk: copies must be deactivated, team must be gone.
    let after_del_personas: Vec<crate::managed_agents::AgentDefinition> = {
        let json = std::fs::read_to_string(&personas_path).unwrap();
        serde_json::from_str(&json).unwrap()
    };
    let after_del_teams: Vec<crate::managed_agents::TeamRecord> = {
        let json = std::fs::read_to_string(&teams_path).unwrap();
        serde_json::from_str(&json).unwrap()
    };
    assert_eq!(after_del_teams.len(), 0, "team must be removed");
    assert!(!after_del_personas[0].is_active, "copy must be deactivated");

    // Step 3: re-add — must reactivate the existing copy.
    let readd = plan_add(
        &after_del_personas,
        &after_del_teams,
        &src,
        &body,
        "2026-07-30T00:00:01Z",
    )
    .expect("re-add must succeed");
    let (after_personas, _) = readd.stores.expect("re-add must produce new stores");
    assert_eq!(after_personas.len(), 1, "no duplicate copies minted");
    assert_eq!(after_personas[0].id, copy_id, "same copy id reused");
    assert!(after_personas[0].is_active, "copy must be reactivated");
}
