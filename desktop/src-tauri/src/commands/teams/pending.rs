//! Retention-store enqueue helpers for the owner's kind:30178 team catalog
//! heads: build and retain a pending projection on share, retain a newer
//! untagged head on unshare, purge + tombstone on delete.
//!
//! Mirrors `commands::personas::pending` one-for-one — same retention store,
//! same monotonic `created_at` rule, same tombstone-first ordering, same
//! flush loop (`flush_pending_events`) as the sole background publisher. The
//! only structural difference is the projection itself: a persona head is
//! built from one record, while a catalog head is built from a team plus its
//! ordered member definitions (`managed_agents::team_catalog`).

use tauri::AppHandle;

use crate::app_state::AppState;
use crate::managed_agents::{
    retention::{RetainedEvent, RetentionScope},
    AgentDefinition, TeamRecord,
};

use buzz_core_pkg::kind::KIND_TEAM_CATALOG;

/// A signed catalog head, retained and awaiting relay acceptance.
pub(super) struct PreparedTeamPublication {
    pub scope: RetentionScope,
    pub event: nostr::Event,
    pub retained: RetainedEvent,
    pub team: TeamRecord,
}

/// Resolve the members of `team` from `personas`, in the team's own
/// membership order.
///
/// Order is load-bearing: it is part of the canonical projection bytes, so
/// resolving through a map would make an unchanged team hash differently on
/// each rebuild. An unresolvable id is an error rather than a skip — silently
/// publishing a team with a member missing would present a different team to
/// the community than the one the owner is looking at, and (A3) the reconcile
/// treats exactly this failure as grounds for retraction.
pub(super) fn resolve_team_members(
    team: &TeamRecord,
    personas: &[AgentDefinition],
) -> Result<Vec<AgentDefinition>, String> {
    team.persona_ids
        .iter()
        .map(|persona_id| {
            personas
                .iter()
                .find(|record| &record.id == persona_id)
                .cloned()
                .ok_or_else(|| format!("team member {persona_id} not found"))
        })
        .collect()
}

/// Whether a retained catalog head carries the exact `shared` tag.
///
/// Reuses `event_is_shared`, the same fail-closed exact-shape check the relay
/// applies at its read gate, so the client's notion of "shared" cannot drift
/// from the relay's.
fn retained_team_is_shared(row: Option<&RetainedEvent>) -> bool {
    use buzz_core_pkg::kind::event_is_shared;
    use nostr::JsonUtil;

    row.and_then(|retained| nostr::Event::from_json(&retained.raw_event).ok())
        .is_some_and(|event| event_is_shared(&event))
}

/// Project each team's catalog visibility from the active relay+owner scope's
/// retained 30178 head.
///
/// Infallible by design, for the same reason as
/// `personas::pending::project_active_persona_sharing`: the scope needs
/// `signing_keys()`, which fails process-wide whenever the identity is lost or
/// the keyring is locked, and propagating that error would break listing,
/// creating, and editing EVERY team. Share state is a view projection, so an
/// unresolvable scope degrades to "not shared" — it can under-report
/// visibility but can never present an unshared team as published.
pub(super) fn project_active_team_sharing(
    app: &AppHandle,
    state: &AppState,
    teams: &mut [TeamRecord],
) {
    let scope = crate::managed_agents::retention::active_retention_scope(app, state);
    project_scoped_team_sharing(scope, teams);
}

fn project_scoped_team_sharing(scope: Result<RetentionScope, String>, teams: &mut [TeamRecord]) {
    let projected = scope.and_then(|scope| {
        project_team_sharing_at(
            &scope.db_path,
            &scope.owner_keys.public_key().to_hex(),
            teams,
        )
    });
    if let Err(error) = projected {
        eprintln!(
            "buzz-desktop: team-share-projection unavailable, reporting every team as unshared: {error}"
        );
        for team in teams {
            team.shared = false;
        }
    }
}

fn project_team_sharing_at(
    db_path: &std::path::Path,
    owner_pubkey: &str,
    teams: &mut [TeamRecord],
) -> Result<(), String> {
    use crate::managed_agents::retention::{get_retained_event, open_retention_db};

    let conn = open_retention_db(db_path)?;
    for team in teams {
        if team.is_builtin {
            team.shared = false;
            continue;
        }
        let retained = get_retained_event(&conn, KIND_TEAM_CATALOG, owner_pubkey, &team.id)?;
        team.shared = retained_team_is_shared(retained.as_ref());
    }
    Ok(())
}

/// Build, sign, and durably retain a team's catalog head in the active
/// relay+owner scope.
///
/// `shared_override` follows the persona rule: the explicit share toggle
/// passes `Some(shared)`, while a rebuild triggered by an edit passes `None`
/// and preserves whatever the scoped head already says. That is what makes an
/// ordinary team edit unable to silently unshare — and it is belt-and-braces
/// here, since share state lives on 30178 and an edit republishes 30176.
pub(super) fn prepare_team_publication(
    app: &AppHandle,
    state: &AppState,
    team: &TeamRecord,
    members: &[AgentDefinition],
    shared_override: Option<bool>,
) -> Result<PreparedTeamPublication, String> {
    let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
    let (event, retained, team) = prepare_team_publication_at(
        &scope.db_path,
        &scope.owner_keys,
        team,
        members,
        shared_override,
    )?;
    Ok(PreparedTeamPublication {
        scope,
        event,
        retained,
        team,
    })
}

pub(super) fn prepare_team_publication_at(
    db_path: &std::path::Path,
    keys: &nostr::Keys,
    team: &TeamRecord,
    members: &[AgentDefinition],
    shared_override: Option<bool>,
) -> Result<(nostr::Event, RetainedEvent, TeamRecord), String> {
    use crate::managed_agents::{
        persona_events::monotonic_created_at,
        retention::{get_retained_event, open_retention_db, retain_event},
        team_catalog::build_team_catalog_event,
    };
    use nostr::JsonUtil;

    let pubkey = keys.public_key().to_hex();
    let conn = open_retention_db(db_path)?;
    let existing = get_retained_event(&conn, KIND_TEAM_CATALOG, &pubkey, &team.id)?;
    let mut scoped_team = team.clone();
    scoped_team.shared =
        shared_override.unwrap_or_else(|| retained_team_is_shared(existing.as_ref()));
    // The size contract runs inside the builder, BEFORE signing, so an
    // oversized team fails here with a named field instead of enqueuing an
    // event the relay would permanently refuse.
    let event = build_team_catalog_event(&scoped_team, members, scoped_team.shared)?
        .custom_created_at(monotonic_created_at(
            existing.as_ref().map(|row| row.created_at),
        ))
        .sign_with_keys(keys)
        .map_err(|e| format!("failed to sign team catalog event: {e}"))?;
    let retained = RetainedEvent {
        kind: KIND_TEAM_CATALOG,
        pubkey,
        d_tag: team.id.clone(),
        content: event.content.to_string(),
        created_at: event.created_at.as_secs() as i64,
        raw_event: event.as_json(),
        pending_sync: true,
    };
    retain_event(&conn, &retained)?;
    Ok((event, retained, scoped_team))
}

/// Purge a deleted team's retained catalog head and enqueue a NIP-09
/// tombstone for its 30178 coordinate.
///
/// The 30176 team head has its own tombstone (`tombstone_team_pending`); this
/// is the catalog counterpart and both run on delete, because the two kinds
/// are separate coordinates and deleting one does not retract the other. Same
/// purge-then-tombstone ordering as personas: removing the 30178 row first
/// under the store lock stops an unpublished re-share from resurrecting the
/// entry after the tombstone lands. Best-effort — a failure is logged and
/// swallowed so a retention hiccup never blocks the disk-authoritative delete.
pub(super) fn tombstone_team_catalog_pending(app: &AppHandle, state: &AppState, d_tag: &str) {
    let result = (|| -> Result<(), String> {
        let scope = crate::managed_agents::retention::active_retention_scope(app, state)?;
        tombstone_team_catalog_at(&scope.db_path, &scope.owner_keys, d_tag)
    })();
    if let Err(e) = result {
        eprintln!("buzz-desktop: team-catalog-tombstone: {e}");
    }
}

/// Scope-free core of [`tombstone_team_catalog_pending`], so the purge and
/// enqueue can be asserted directly against a retention database.
pub(super) fn tombstone_team_catalog_at(
    db_path: &std::path::Path,
    keys: &nostr::Keys,
    d_tag: &str,
) -> Result<(), String> {
    use crate::managed_agents::{
        retention::{
            delete_retained_event, open_retention_db, retain_event, tombstone_retention_d_tag,
        },
        team_catalog::build_team_catalog_delete,
    };
    use nostr::JsonUtil;

    const KIND_DELETE: u32 = 5;

    let pubkey = keys.public_key().to_hex();
    let event = build_team_catalog_delete(d_tag, &pubkey)?
        .sign_with_keys(keys)
        .map_err(|e| format!("failed to sign team catalog tombstone: {e}"))?;
    let conn = open_retention_db(db_path)?;
    delete_retained_event(&conn, KIND_TEAM_CATALOG, &pubkey, d_tag)?;
    retain_event(
        &conn,
        &RetainedEvent {
            kind: KIND_DELETE,
            pubkey,
            // Key by the target coordinate so the 30176 and 30178
            // tombstones for one team occupy distinct rows (F2c).
            d_tag: tombstone_retention_d_tag(KIND_TEAM_CATALOG, d_tag),
            content: event.content.to_string(),
            created_at: event.created_at.as_secs() as i64,
            raw_event: event.as_json(),
            pending_sync: true,
        },
    )
}

#[cfg(test)]
mod tests;
