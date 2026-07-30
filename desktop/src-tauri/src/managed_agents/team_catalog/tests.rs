use super::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
        instructions: Some("Coordinate carefully.".to_string()),
        persona_ids: vec!["m1".to_string(), "m2".to_string()],
        is_builtin: false,
        shared: false,
        source_dir: Some(PathBuf::from("/local/only/path")),
        is_symlink: true,
        symlink_target: Some("/somewhere/private".to_string()),
        version: Some("1.0".to_string()),
        created_at: "2026-07-30T00:00:00Z".to_string(),
        updated_at: "2026-07-30T00:00:00Z".to_string(),
    }
}

// ── Field discipline / anti-leak ─────────────────────────────────────────────

#[test]
fn test_projection_omits_local_only_team_fields() {
    let content = build_team_catalog_content(&team(), &[member("m1", "One")]).unwrap();
    let json = team_catalog_content_json(&content).unwrap();

    assert!(json.contains("\"name\":\"Catalog Team\""));
    for local_only in [
        "source_dir",
        "is_symlink",
        "symlink_target",
        "is_builtin",
        "version",
        "created_at",
        "updated_at",
        "persona_ids",
    ] {
        assert!(
            !json.contains(local_only),
            "local-only field '{local_only}' must never be projected"
        );
    }
}

#[test]
fn test_projection_never_contains_a_source_allowlist_pubkey() {
    // The exact leak the sanitizer exists to prevent: an allowlist entry is a
    // real pubkey the owner chose to trust.
    const SECRET_PEER: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let mut one = member("m1", "One");
    one.respond_to = Some(RespondTo::Allowlist.as_str().to_string());
    one.respond_to_allowlist = vec![SECRET_PEER.to_string()];
    one.env_vars
        .insert("API_TOKEN".to_string(), "super-secret".to_string());

    let content = build_team_catalog_content(&team(), &[one]).unwrap();
    let json = team_catalog_content_json(&content).unwrap();

    assert!(!json.contains(SECRET_PEER), "allowlist pubkey leaked");
    assert!(!json.contains("super-secret"), "env var value leaked");
    assert!(!json.contains("API_TOKEN"), "env var key leaked");
    assert!(!json.contains("respond_to_allowlist"));
}

#[test]
fn test_allowlist_mode_downgrades_to_owner_only_not_an_empty_allowlist() {
    // Projecting `allowlist` with no entries would read, on the recipient, as
    // a mode whose trust list is empty — the sanitizer must downgrade the mode
    // itself rather than emptying the list.
    let mut one = member("m1", "One");
    one.respond_to = Some(RespondTo::Allowlist.as_str().to_string());
    one.respond_to_allowlist = vec!["a".repeat(64)];

    let content = build_team_catalog_content(&team(), &[one]).unwrap();

    assert_eq!(
        content.members[0].respond_to.as_deref(),
        Some(RespondTo::OwnerOnly.as_str())
    );
}

#[test]
fn test_non_allowlist_respond_to_modes_are_projected_verbatim() {
    for mode in [RespondTo::OwnerOnly, RespondTo::Anyone] {
        let mut one = member("m1", "One");
        one.respond_to = Some(mode.as_str().to_string());
        let content = build_team_catalog_content(&team(), &[one]).unwrap();
        assert_eq!(
            content.members[0].respond_to.as_deref(),
            Some(mode.as_str())
        );
    }
}

#[test]
fn test_parallelism_is_clamped_into_the_supported_range() {
    for (input, expected) in [(0u32, 1u32), (1, 1), (32, 32), (9_999, 32)] {
        let mut one = member("m1", "One");
        one.parallelism = Some(input);
        let content = build_team_catalog_content(&team(), &[one]).unwrap();
        assert_eq!(content.members[0].parallelism, Some(expected));
    }
}

// ── Member resolution ───────────────────────────────────────────────────────

#[test]
fn test_members_resolve_in_team_membership_order() {
    let personas = vec![member("m2", "Two"), member("m1", "One")];

    let resolved = resolve_team_members(&team(), &personas).unwrap();

    let ids: Vec<&str> = resolved.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["m1", "m2"],
        "order is part of the canonical bytes, so it follows the team, not the store"
    );
}

#[test]
fn test_unresolvable_member_fails_resolution_rather_than_being_skipped() {
    let error = resolve_team_members(&team(), &[member("m1", "One")]).unwrap_err();

    assert!(error.contains("team member m2 not found"));
}

// ── Determinism (the reconcile depends on it) ───────────────────────────────

#[test]
fn test_rebuilding_an_unchanged_team_reproduces_identical_bytes() {
    // The freshness reconcile republishes on a byte mismatch, so a builder
    // that is not deterministic would republish the whole catalog every boot.
    let members = [member("m1", "One"), member("m2", "Two")];
    let first = build_team_catalog_content(&team(), &members).unwrap();
    let second = build_team_catalog_content(&team(), &members).unwrap();

    assert_eq!(
        team_catalog_content_json(&first),
        team_catalog_content_json(&second)
    );
}

#[test]
fn test_member_order_is_part_of_the_canonical_bytes() {
    let forward = [member("m1", "One"), member("m2", "Two")];
    let reversed = [member("m2", "Two"), member("m1", "One")];

    let a = build_team_catalog_content(&team(), &forward).unwrap();
    let b = build_team_catalog_content(&team(), &reversed).unwrap();

    assert_ne!(team_catalog_content_json(&a), team_catalog_content_json(&b));
}

#[test]
fn test_editing_a_member_definition_changes_the_team_bytes() {
    let before = build_team_catalog_content(&team(), &[member("m1", "One")]).unwrap();
    let mut edited = member("m1", "One");
    edited.system_prompt = "Do the work differently.".to_string();
    let after = build_team_catalog_content(&team(), &[edited]).unwrap();

    assert_ne!(
        team_catalog_content_json(&before),
        team_catalog_content_json(&after)
    );
}

// ── Built-in reuse hint (exact-hash gated) ──────────────────────────────────

#[test]
fn test_builtin_member_carries_slug_and_projection_hash() {
    let mut builtin = member("m1", "One");
    builtin.is_builtin = true;
    builtin.source_team_persona_slug = Some("reviewer".to_string());

    let content = build_team_catalog_content(&team(), &[builtin]).unwrap();
    let projected = &content.members[0];

    assert_eq!(projected.builtin_slug.as_deref(), Some("reviewer"));
    assert!(projected.projection_hash.is_some());
}

#[test]
fn test_non_builtin_member_carries_no_reuse_hint() {
    let content = build_team_catalog_content(&team(), &[member("m1", "One")]).unwrap();

    assert_eq!(content.members[0].builtin_slug, None);
    assert_eq!(content.members[0].projection_hash, None);
}

#[test]
fn test_reuse_hash_changes_when_the_builtin_definition_changes() {
    // Same slug, different definition — the recipient must be able to detect
    // it and fall back rather than substituting a mismatched local built-in.
    let mut original = member("m1", "One");
    original.is_builtin = true;
    original.source_team_persona_slug = Some("reviewer".to_string());
    let mut changed = original.clone();
    changed.system_prompt = "Review differently.".to_string();

    let a = build_team_catalog_content(&team(), &[original]).unwrap();
    let b = build_team_catalog_content(&team(), &[changed]).unwrap();

    assert_eq!(
        a.members[0].builtin_slug, b.members[0].builtin_slug,
        "the slug is unchanged, which is exactly why the hash must differ"
    );
    assert_ne!(a.members[0].projection_hash, b.members[0].projection_hash);
}

#[test]
fn test_reuse_hash_excludes_the_hint_fields_so_a_recipient_can_recompute_it() {
    // The recipient hashes its OWN local built-in, which knows nothing about
    // the publisher's slug. A self-referential hash could never match.
    let mut builtin = member("m1", "One");
    builtin.is_builtin = true;
    builtin.source_team_persona_slug = Some("reviewer".to_string());
    let content = build_team_catalog_content(&team(), &[builtin]).unwrap();
    let projected = &content.members[0];

    let mut hint_free = projected.clone();
    hint_free.builtin_slug = None;
    hint_free.projection_hash = None;

    assert_eq!(
        projected.projection_hash.as_deref(),
        Some(member_projection_hash(&hint_free).as_str())
    );
}

// ── Size contract ───────────────────────────────────────────────────────────

#[test]
fn test_member_count_at_the_limit_is_accepted_and_one_over_is_rejected() {
    let at_limit: Vec<AgentDefinition> = (0..MAX_MEMBERS)
        .map(|i| member(&format!("m{i}"), &format!("Member {i}")))
        .collect();
    assert!(build_team_catalog_content(&team(), &at_limit).is_ok());

    let mut over = at_limit;
    over.push(member("extra", "Extra"));
    let error = build_team_catalog_content(&team(), &over).unwrap_err();
    assert!(error.contains("team too large to share"), "{error}");
    assert!(error.contains("65 members"), "{error}");
}

#[test]
fn test_oversized_avatar_is_rejected_and_names_the_member() {
    let mut one = member("m1", "Avatar Hog");
    one.avatar_url = Some("d".repeat(MAX_AVATAR_URL_BYTES + 1));

    let error = build_team_catalog_content(&team(), &[one]).unwrap_err();

    assert!(error.contains("team too large to share"), "{error}");
    assert!(error.contains("Avatar Hog"), "{error}");
}

#[test]
fn test_avatar_exactly_at_the_limit_is_accepted() {
    let mut one = member("m1", "One");
    one.avatar_url = Some("d".repeat(MAX_AVATAR_URL_BYTES));
    assert!(build_team_catalog_content(&team(), &[one]).is_ok());
}

#[test]
fn test_many_legal_members_still_reject_on_the_total_ceiling() {
    // The amplification case: every member is individually within bounds, but
    // the event as a whole would exceed the relay's ingest ceiling.
    let members: Vec<AgentDefinition> = (0..MAX_MEMBERS)
        .map(|i| {
            let mut one = member(&format!("m{i}"), &format!("Member {i}"));
            one.system_prompt = "p".repeat(MAX_SYSTEM_PROMPT_BYTES);
            one
        })
        .collect();

    let error = build_team_catalog_content(&team(), &members).unwrap_err();

    assert!(error.contains("the projection is"), "{error}");
    assert!(
        !error.contains("members (limit"),
        "the per-field bounds all pass; the total is what rejects: {error}"
    );
}

#[test]
fn test_the_total_ceiling_stays_under_the_relay_ingest_limit() {
    // The contract only means anything if an accepted projection is one the
    // relay will actually take (`MAX_EVENT_CONTENT_BYTES` = 256 KiB).
    const { assert!(MAX_TOTAL_BYTES < 256 * 1024) };
}

#[test]
fn test_oversized_team_text_fields_are_rejected() {
    for (label, subject) in [
        ("the team name", {
            let mut t = team();
            t.name = "n".repeat(MAX_NAME_BYTES + 1);
            t
        }),
        ("the team description", {
            let mut t = team();
            t.description = Some("d".repeat(MAX_TEXT_BYTES + 1));
            t
        }),
        ("the team instructions", {
            let mut t = team();
            t.instructions = Some("i".repeat(MAX_TEXT_BYTES + 1));
            t
        }),
    ] {
        let error = build_team_catalog_content(&subject, &[member("m1", "One")]).unwrap_err();
        assert!(error.contains(label), "expected '{label}' in: {error}");
    }
}

#[test]
fn test_oversized_name_pool_is_rejected() {
    let mut one = member("m1", "Pool Hog");
    one.name_pool = (0..=MAX_NAME_POOL_ENTRIES).map(|i| i.to_string()).collect();

    let error = build_team_catalog_content(&team(), &[one]).unwrap_err();

    assert!(error.contains("name-pool entries"), "{error}");
}

#[test]
fn test_an_empty_team_projects_successfully() {
    let content = build_team_catalog_content(&team(), &[]).unwrap();

    assert!(content.members.is_empty());
    // `members` is not `skip_serializing_if`, so an empty team is explicit
    // rather than indistinguishable from an omitted field.
    assert!(team_catalog_content_json(&content)
        .unwrap()
        .contains("\"members\":[]"));
}

// ── Event shape ─────────────────────────────────────────────────────────────

#[test]
fn test_event_uses_kind_30178_and_the_team_id_as_its_d_tag() {
    let event = build_team_catalog_event(&team(), &[member("m1", "One")], false)
        .unwrap()
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();

    assert_eq!(event.kind.as_u16() as u32, KIND_TEAM_CATALOG);
    let d_tags: Vec<&str> = event
        .tags
        .iter()
        .filter_map(|tag| {
            let parts = tag.as_slice();
            (parts.first().map(String::as_str) == Some("d")).then(|| parts[1].as_str())
        })
        .collect();
    // The relay rejects anything but exactly one bounded `d` tag.
    assert_eq!(d_tags, vec!["team-abc"]);
}

#[test]
fn test_shared_tag_is_present_only_when_sharing() {
    for shared in [true, false] {
        let event = build_team_catalog_event(&team(), &[member("m1", "One")], shared)
            .unwrap()
            .sign_with_keys(&nostr::Keys::generate())
            .unwrap();

        assert_eq!(
            buzz_core_pkg::kind::event_is_shared(&event),
            shared,
            "the relay read gate keys off this tag"
        );
    }
}

#[test]
fn test_oversized_team_fails_before_an_event_is_ever_built() {
    // The whole point of pre-enqueue validation: no signed event exists to be
    // durably queued and then permanently refused by the relay.
    let mut one = member("m1", "One");
    one.avatar_url = Some("d".repeat(MAX_AVATAR_URL_BYTES + 1));

    assert!(build_team_catalog_event(&team(), &[one], true).is_err());
}

// ── Inbound parsing ─────────────────────────────────────────────────────────

fn signed_event_with_content(content: &str) -> nostr::Event {
    EventBuilder::new(Kind::Custom(KIND_TEAM_CATALOG as u16), content)
        .tags(vec![Tag::parse(["d", "team-abc"]).unwrap()])
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap()
}

#[test]
fn test_content_round_trips_through_an_event() {
    let members = [member("m1", "One"), member("m2", "Two")];
    let built = build_team_catalog_content(&team(), &members).unwrap();
    let event = build_team_catalog_event(&team(), &members, true)
        .unwrap()
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();

    assert_eq!(team_catalog_content_from_event(&event).unwrap(), built);
}

#[test]
fn test_unknown_schema_version_is_rejected() {
    let event = signed_event_with_content(r#"{"v":2,"name":"Future Team","members":[]}"#);

    let error = team_catalog_content_from_event(&event).unwrap_err();

    assert!(
        error.contains("unsupported team catalog schema version 2"),
        "{error}"
    );
}

#[test]
fn test_body_missing_the_version_is_rejected() {
    // `v` has no serde default, so a body without it cannot masquerade as v1.
    let event = signed_event_with_content(r#"{"name":"No Version","members":[]}"#);

    assert!(team_catalog_content_from_event(&event).is_err());
}

#[test]
fn test_malformed_member_fields_are_rejected() {
    // A hostile publisher can put anything on the wire; a wrong-typed field
    // must fail parsing rather than silently coercing.
    let event = signed_event_with_content(
        r#"{"v":1,"name":"Bad","members":[{"member_key":"m1","display_name":"One","parallelism":"lots"}]}"#,
    );

    assert!(team_catalog_content_from_event(&event).is_err());
}

#[test]
fn test_inbound_body_over_the_size_contract_is_rejected_on_read() {
    // Readers enforce the same bounds as writers: a publisher that bypassed
    // our builder must not be able to hand us an unbounded body to render.
    let members: String = (0..=MAX_MEMBERS)
        .map(|i| format!(r#"{{"member_key":"m{i}","display_name":"M{i}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let event = signed_event_with_content(&format!(
        r#"{{"v":1,"name":"Too Many","members":[{members}]}}"#
    ));

    let error = team_catalog_content_from_event(&event).unwrap_err();

    assert!(error.contains("team too large to share"), "{error}");
}

#[test]
fn test_member_key_is_stable_for_an_unchanged_member() {
    let one = member("m1", "One");
    let a = build_team_catalog_content(&team(), std::slice::from_ref(&one)).unwrap();
    let b = build_team_catalog_content(&team(), &[one]).unwrap();

    assert_eq!(a.members[0].member_key, b.members[0].member_key);
    assert!(!a.members[0].member_key.is_empty());
}

// ── Tombstone ───────────────────────────────────────────────────────────────

#[test]
fn test_catalog_delete_targets_the_30178_coordinate_with_no_e_tag() {
    const OWNER: &str = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    let event = build_team_catalog_delete("team-abc", OWNER)
        .unwrap()
        .sign_with_keys(&nostr::Keys::generate())
        .unwrap();

    assert_eq!(event.kind, Kind::Custom(5));
    let a_tags: Vec<&[String]> = event
        .tags
        .iter()
        .map(|tag| tag.as_slice())
        .filter(|parts| parts.first().map(String::as_str) == Some("a"))
        .collect();
    assert_eq!(a_tags.len(), 1);
    assert_eq!(
        a_tags[0][1],
        format!("{KIND_TEAM_CATALOG}:{OWNER}:team-abc")
    );
    // An e-tag would leave the replaceable coordinate live.
    assert!(event
        .tags
        .iter()
        .all(|tag| tag.as_slice().first().map(String::as_str) != Some("e")));
}
