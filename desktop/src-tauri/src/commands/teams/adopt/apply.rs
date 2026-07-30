//! The store-mutation half of `add_team_from_catalog`: turn a verified
//! projection into local records with byte-level rollback on error.
//!
//! [`plan_add`] computes both stores in memory before anything is written, so
//! no failure in resolving members can leave a half-added team on disk. Only
//! the two saves themselves remain. Before either write we snapshot the raw
//! bytes of both files (or record their absence). When either save fails we
//! restore both snapshots, replacing the on-disk content exactly as it was —
//! including for a reactivated member copy where logical undo would require a
//! field revert with no row to delete.
//!
//! **Crash window.** A process kill between the first commit and the second
//! (or between the second and a successful restore) leaves the stores
//! inconsistent. On restart the team either exists without some member copies,
//! or the member copies exist without the team. The next add of the same
//! publication is idempotent: the replay check in `plan_add` finds the team
//! (if present) and returns it; any orphaned copies are reused by provenance
//! matching. Users who encounter a crash mid-add should retry the add.

use std::path::Path;

use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::{
    app_state::AppState,
    managed_agents::{
        atomic_write_json_restricted, load_personas, load_teams, managed_agents_store_path,
        save_personas, save_teams,
        team_catalog::{
            builtin_catalog_slug, local_member_projection_hash, TeamCatalogContent,
            TeamCatalogMember,
        },
        teams_store_path, try_regenerate_nest, AgentDefinition, RespondTo, TeamCatalogSource,
        TeamMemberCatalogSource, TeamRecord,
    },
    util::now_iso,
};

use super::AddTeamFromCatalogResult;

/// The complete post-add state of both stores, plus the team to report.
///
/// `stores` is `None` when nothing needs writing — the replay case.
#[derive(Debug)]
pub(super) struct AddPlan {
    pub stores: Option<(Vec<AgentDefinition>, Vec<TeamRecord>)>,
    pub team: TeamRecord,
}

/// Raw bytes of a file before the add, or `None` if the file did not exist.
///
/// Used to restore both stores to their exact pre-add state when either save
/// fails. Restoring the managed-agents store from bytes is the only way to
/// undo a reactivation (flipping `is_active` from `false` to `true`) without
/// re-reading the file and re-applying the mutation in reverse.
type RawSnapshot = Option<Vec<u8>>;

/// Read the raw bytes of `path`, or `None` if the file does not yet exist.
fn snapshot(path: &Path) -> Result<RawSnapshot, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to snapshot {}: {e}", path.display())),
    }
}

/// Restore `path` from a snapshot taken by [`snapshot`].
///
/// `None` means the file was absent before the add; remove it so the on-disk
/// state matches. A restore failure is not masked — it is returned alongside
/// the original cause that triggered the rollback.
fn restore(path: &Path, snap: RawSnapshot) -> Result<(), String> {
    match snap {
        Some(bytes) => atomic_write_json_restricted(path, &bytes),
        None => std::fs::remove_file(path)
            .map_err(|e| format!("failed to remove {} during restore: {e}", path.display())),
    }
}

/// Write both stores with byte-level rollback on failure.
///
/// Takes the file paths and write callbacks as arguments so the logic is
/// testable with temporary files and injected-failure writers without needing
/// a Tauri `AppHandle`.
///
/// On any write error: restore both files from the pre-call snapshots. A
/// restore failure is surfaced alongside the original error, not masked.
pub(super) fn commit_stores(
    personas_path: &Path,
    teams_path: &Path,
    write_personas: impl FnOnce() -> Result<(), String>,
    write_teams: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let personas_snap = snapshot(personas_path)?;
    let teams_snap = snapshot(teams_path)?;

    if let Err(error) = write_personas().and_then(|()| write_teams()) {
        let restore_result =
            restore(personas_path, personas_snap).and_then(|()| restore(teams_path, teams_snap));
        if let Err(restore_error) = restore_result {
            return Err(format!(
                "{error} (and the local stores could not be restored: {restore_error})"
            ));
        }
        return Err(error);
    }

    Ok(())
}

pub(super) fn add_verified_team(
    app: &AppHandle,
    source: &TeamCatalogSource,
    content: &TeamCatalogContent,
) -> Result<AddTeamFromCatalogResult, String> {
    let state = app.state::<AppState>();
    // Held across load, plan, and save: the replay check is only meaningful if
    // no concurrent add of the same coordinate can interleave between reading
    // the teams and writing them back.
    let _store_guard = state
        .managed_agents_store_lock
        .lock()
        .map_err(|error| error.to_string())?;

    let personas_path = managed_agents_store_path(app)?;
    let teams_path = teams_store_path(app)?;

    let personas_before = load_personas(app)?;
    let teams_before = load_teams(app)?;
    let plan = plan_add(&personas_before, &teams_before, source, content, &now_iso())?;

    let Some((personas, teams)) = plan.stores else {
        return Ok(AddTeamFromCatalogResult {
            team: plan.team,
            already_present: true,
        });
    };

    // Snapshot raw bytes before either write. If either save fails, we restore
    // both files to exactly their pre-add state. Using raw bytes rather than
    // re-serializing the before-vectors ensures the restore is byte-exact even
    // for a reactivated member copy whose logical undo is a field revert.
    commit_stores(
        &personas_path,
        &teams_path,
        || save_personas(app, &personas),
        || save_teams(app, &teams),
    )?;

    try_regenerate_nest(app);
    Ok(AddTeamFromCatalogResult {
        team: plan.team,
        already_present: false,
    })
}

/// Compute both stores as they will be after the add. Pure — no I/O, so every
/// resolution rule below is testable without a Tauri app or a relay.
pub(super) fn plan_add(
    personas_before: &[AgentDefinition],
    teams_before: &[TeamRecord],
    source: &TeamCatalogSource,
    content: &TeamCatalogContent,
    now: &str,
) -> Result<AddPlan, String> {
    // Replay: the same publication added twice returns the team already held
    // instead of minting a second copy.
    if let Some(existing) = teams_before
        .iter()
        .find(|team| team.catalog_source.as_ref() == Some(source))
    {
        return Ok(AddPlan {
            stores: None,
            team: existing.clone(),
        });
    }

    let mut personas = personas_before.to_vec();
    let persona_ids = content
        .members
        .iter()
        .map(|member| resolve_member(&mut personas, source, member, now))
        .collect::<Result<Vec<_>, _>>()?;
    let team = TeamRecord {
        id: Uuid::new_v4().to_string(),
        name: content.name.clone(),
        description: content.description.clone(),
        instructions: content.instructions.clone(),
        persona_ids,
        is_builtin: false,
        // A copy is not published. Sharing it is a separate, explicit act by
        // its new owner, at their own coordinate.
        shared: false,
        catalog_source: Some(source.clone()),
        source_dir: None,
        is_symlink: false,
        symlink_target: None,
        version: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    };

    let mut teams = teams_before.to_vec();
    teams.push(team.clone());
    Ok(AddPlan {
        stores: Some((personas, teams)),
        team,
    })
}

/// Resolve one published member to a local persona id, adding or reactivating
/// a record as needed. Returns the local id to put in the team's membership.
fn resolve_member(
    personas: &mut Vec<AgentDefinition>,
    source: &TeamCatalogSource,
    member: &TeamCatalogMember,
    now: &str,
) -> Result<String, String> {
    if let Some(local_id) = reusable_builtin(personas, member) {
        return Ok(local_id);
    }
    if let Some(existing) = personas
        .iter_mut()
        .find(|persona| member_provenance_matches(persona, source, member))
    {
        // A copy of this exact member version already exists, from an earlier
        // add of this same publication. Reuse it, reactivating if a prior team
        // delete left it inactive. Reuse is deliberately NOT extended across
        // publications: two teams by one publisher that embed an identical
        // member get one copy each, so deleting either team cannot orphan a
        // record the other still points at.
        if !existing.is_active {
            existing.is_active = true;
            existing.updated_at = now.to_string();
        }
        return Ok(existing.id.clone());
    }
    let copy = member_copy(source, member, now)?;
    let id = copy.id.clone();
    personas.push(copy);
    Ok(id)
}

/// A local built-in that is byte-identical to the published member.
///
/// Substitution requires BOTH the canonical `builtin:<slug>` to exist locally
/// AND the local built-in's current projection hash to equal the published
/// `projection_hash`. The hash is what makes the hint exact-match gated: a
/// hostile `builtin_slug` paired with unrelated embedded fields, a retired
/// slug, and a slug whose local definition has since changed all fail the
/// comparison and fall through to an ordinary copy built from the embedded
/// fields — which are authoritative.
fn reusable_builtin(personas: &[AgentDefinition], member: &TeamCatalogMember) -> Option<String> {
    let slug = member.builtin_slug.as_deref()?;
    let published_hash = member.projection_hash.as_deref()?;
    personas
        .iter()
        .find(|persona| {
            builtin_catalog_slug(persona) == Some(slug)
                && local_member_projection_hash(persona) == published_hash
        })
        .map(|persona| persona.id.clone())
}

/// Whether a local persona is a copy of exactly this published member.
///
/// All four components must match. Dropping `projection_hash` would make two
/// versions of one published member collapse onto a single mutable local
/// record, so adding the newer team would silently rewrite the copy the older
/// team is still using.
fn member_provenance_matches(
    persona: &AgentDefinition,
    source: &TeamCatalogSource,
    member: &TeamCatalogMember,
) -> bool {
    persona.team_catalog_source.as_ref().is_some_and(|held| {
        held.owner_pubkey == source.owner_pubkey
            && held.team_d_tag == source.team_d_tag
            && held.member_key == member.member_key
            && held.projection_hash == member_version_hash(member)
    })
}

/// The version stamp stored on a copy.
///
/// A publisher-supplied `projection_hash` is only present on built-in reuse
/// hints, so it cannot serve as the version for ordinary members — and it is
/// publisher-controlled either way. Recomputing it locally over the member as
/// published makes the stamp mean "this exact projection", uniformly, for
/// every member.
fn member_version_hash(member: &TeamCatalogMember) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(member).unwrap_or_default();
    hex::encode(Sha256::digest(&json))
}

/// Build a local persona from a published member's embedded fields.
///
/// Embedding is authoritative: every field comes from the projection, never
/// from a local record that happens to share a name. Fields absent from the
/// projection by design — env vars, allowlist pubkeys — are absent here too,
/// so a copy starts with no inherited secrets and no inherited audience.
fn member_copy(
    source: &TeamCatalogSource,
    member: &TeamCatalogMember,
    now: &str,
) -> Result<AgentDefinition, String> {
    Ok(AgentDefinition {
        id: Uuid::new_v4().to_string(),
        display_name: member.display_name.clone(),
        avatar_url: member.avatar_url.clone(),
        system_prompt: member.system_prompt.clone().unwrap_or_default(),
        runtime: member.runtime.clone(),
        model: member.model.clone(),
        provider: member.provider.clone(),
        name_pool: member.name_pool.clone(),
        is_builtin: false,
        is_active: true,
        shared: false,
        source_team: None,
        source_team_persona_slug: None,
        catalog_source: None,
        team_catalog_source: Some(TeamMemberCatalogSource {
            owner_pubkey: source.owner_pubkey.clone(),
            team_d_tag: source.team_d_tag.clone(),
            member_key: member.member_key.clone(),
            projection_hash: member_version_hash(member),
        }),
        env_vars: Default::default(),
        // Validated at the boundary rather than copied opaquely: an
        // unrecognized mode from a foreign publisher must not become a local
        // definition whose audience differs from what the recipient sees.
        respond_to: member
            .respond_to
            .as_deref()
            .map(|mode| RespondTo::parse_wire(mode).map(|_| mode.to_string()))
            .transpose()?,
        respond_to_allowlist: Vec::new(),
        parallelism: member.parallelism,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    })
}
