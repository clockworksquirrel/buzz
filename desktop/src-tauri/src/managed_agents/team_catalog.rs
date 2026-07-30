//! Project a `TeamRecord` plus its member definitions onto a kind:30178 team
//! catalog event.
//!
//! Kind 30176 (`team_events`) is the team's own wire body: membership by local
//! persona id, meaningless to anyone but the owner. Kind 30178 is the
//! *catalog projection* — a self-contained, shareable snapshot that embeds
//! every member's safe definition so a recipient can rebuild the team without
//! reading the owner's personas. The two are deliberately separate kinds: an
//! ordinary team edit republishes 30176 and cannot disturb catalog share
//! state, which lives only on the 30178 head's `shared` tag.
//!
//! This module is a pure builder plus validator — it performs no I/O and owns
//! no wiring. Publication, unshare, and tombstone live in
//! `commands::teams::sharing`.
//!
//! Field discipline is inherited from `persona_events::PersonaEventContent`:
//! an explicit opt-IN projection over the persona-catalog safe set. Env vars,
//! respond-to allowlist pubkeys, local ids, filesystem paths, and every other
//! install-specific or secret-bearing field are structurally absent from the
//! projection types below, so no future `AgentDefinition` field can leak by
//! being forgotten.

use buzz_core_pkg::kind::KIND_TEAM_CATALOG;
use nostr::{EventBuilder, Kind, Tag};
use serde::{Deserialize, Serialize};

use super::{persona_events::persona_d_tag, AgentDefinition, RespondTo, TeamRecord};

/// Schema version of the 30178 content body. A reader that does not recognize
/// the value must refuse the event rather than guess at its shape.
pub const TEAM_CATALOG_SCHEMA_VERSION: u32 = 1;

// ── Size contract ────────────────────────────────────────────────────────────
//
// A 30178 event amplifies N member definitions into ONE event, so bounds that
// are immaterial for a single kind:30175 persona become load-bearing here: 64
// members each carrying a 64 KiB avatar would be a 4 MiB event. The relay's
// ingest ceiling is 256 KiB (`MAX_EVENT_CONTENT_BYTES`,
// `crates/buzz-relay/src/handlers/ingest.rs`), and an event that exceeds it is
// rejected AFTER being signed and durably enqueued — a permanently stuck
// pending row with no user-visible cause. Every bound below is therefore
// enforced BEFORE the event is built, so the failure surfaces synchronously at
// share time as a deterministic error instead of a silent queue.
//
// `MAX_TOTAL_BYTES` is the only bound that matters for relay acceptance; the
// per-field bounds exist so an oversized team names the specific field that
// pushed it over instead of reporting an opaque total.

/// Maximum members in one catalog projection.
pub const MAX_MEMBERS: usize = 64;
/// Maximum bytes for a team or member display name.
pub const MAX_NAME_BYTES: usize = 256;
/// Maximum bytes for the team description and team instructions.
pub const MAX_TEXT_BYTES: usize = 4 * 1024;
/// Maximum bytes for a member's system prompt.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 16 * 1024;
/// Maximum bytes for a member's avatar URL. Generous because the persona
/// catalog permits inline emoji data URLs, not just `https://` links.
pub const MAX_AVATAR_URL_BYTES: usize = 32 * 1024;
/// Maximum entries in a member's name pool.
pub const MAX_NAME_POOL_ENTRIES: usize = 64;
/// Maximum bytes for the whole serialized content body. Held well under the
/// relay's 256 KiB ingest ceiling so base64/transport overhead upstream of the
/// check cannot turn an accepted projection into a rejected event.
pub const MAX_TOTAL_BYTES: usize = 192 * 1024;

/// The JSON body stored in a kind:30178 event's content field.
///
/// Field order is pinned by the struct declaration: serde emits in declaration
/// order, so a reorder changes the content bytes and the NIP-01 event id — and
/// the freshness reconcile compares exactly those bytes to decide whether a
/// shared projection is stale. Reordering would make every shared team look
/// stale exactly once, republishing the entire catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCatalogContent {
    /// Schema version. First field so a reader can dispatch on it before
    /// committing to the rest of the shape.
    pub v: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Member projections in the team's own membership order. Order is part of
    /// the canonical bytes, so a membership reorder is a genuine change and
    /// correctly republishes.
    pub members: Vec<TeamCatalogMember>,
}

/// One member's safe definition, embedded in full.
///
/// Embedding is authoritative: a recipient can always rebuild this member from
/// these fields alone. `builtin_slug` / `projection_hash` are a reuse *hint*
/// and never an identity authority — see their doc comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCatalogMember {
    /// Stable, opaque identity of this member WITHIN this team publication.
    ///
    /// Provenance for an added member is the triple `(owner_pubkey,
    /// team_d_tag, member_key)`. The value is derived from the source
    /// persona's normalized d-tag so that rebuilding an unchanged team
    /// reproduces identical bytes — a random key would make every reconcile
    /// pass see a hash mismatch and republish forever.
    ///
    /// A recipient MUST treat it as opaque and MUST NOT resolve it as a
    /// kind:30175 coordinate in the publisher's namespace: the publisher may
    /// never have shared that persona individually, and a member that is
    /// present here is not thereby readable there.
    pub member_key: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub name_pool: Vec<String>,
    /// Sanitized audience mode. `allowlist` is never projected — see
    /// [`sanitized_respond_to`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respond_to: Option<String>,
    /// Clamped to 1..=32 at projection time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<u32>,
    /// Reuse hint: the built-in slug this member was installed from.
    ///
    /// Present only for built-in members. A recipient may substitute its own
    /// local built-in ONLY when the slug exists locally AND that built-in's
    /// current projection hash equals `projection_hash`. Any mismatch — a
    /// retired slug, a changed prompt, or a hostile slug paired with unrelated
    /// embedded fields — falls back to an ordinary copy built from the
    /// embedded fields above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_slug: Option<String>,
    /// Hash of this member's own embedded projection. Meaningful only
    /// alongside `builtin_slug`; it is what makes the reuse hint exact-match
    /// gated rather than name-trusting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_hash: Option<String>,
}

/// Resolve the members of `team` from `personas`, in the team's own
/// membership order.
///
/// Order is load-bearing: it is part of the canonical projection bytes, so
/// resolving through a map would make an unchanged team rebuild to different
/// bytes each time. An unresolvable id is an error rather than a skip —
/// silently publishing a team with a member missing would present a different
/// team to the community than the one the owner is looking at, and the
/// freshness reconcile treats exactly this failure as grounds for retraction.
pub fn resolve_team_members(
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

/// There is no `respond_to_allowlist` field on [`TeamCatalogMember`], and that
/// absence is the anti-leak guarantee: an allowlist is a list of real pubkeys
/// the owner chose to trust, and publishing it to a community catalog would
/// disclose the owner's social graph. Rather than projecting an emptied list —
/// which would silently widen the member's audience for a recipient who reads
/// `allowlist` mode with no entries — the mode itself is downgraded to
/// `owner-only`, the most restrictive setting. A recipient that wants an
/// allowlist must author one.
fn sanitized_respond_to(record: &AgentDefinition) -> Option<String> {
    match record.respond_to.as_deref() {
        Some(mode) if mode == RespondTo::Allowlist.as_str() => {
            Some(RespondTo::OwnerOnly.as_str().to_string())
        }
        other => other.map(str::to_string),
    }
}

/// Project one member definition, without the built-in reuse hint.
fn member_projection(record: &AgentDefinition) -> TeamCatalogMember {
    TeamCatalogMember {
        member_key: persona_d_tag(record),
        display_name: record.display_name.clone(),
        // Mirrors `persona_event_content`: always `Some`, including for an
        // empty prompt, so the encoding does not depend on emptiness.
        system_prompt: Some(record.system_prompt.clone()),
        avatar_url: record.avatar_url.clone(),
        runtime: record.runtime.clone(),
        model: record.model.clone(),
        provider: record.provider.clone(),
        name_pool: record.name_pool.clone(),
        respond_to: sanitized_respond_to(record),
        parallelism: record.parallelism.map(|value| value.clamp(1, 32)),
        builtin_slug: None,
        projection_hash: None,
    }
}

/// Project a member and attach the built-in reuse hint when applicable.
///
/// The hash is computed over the member projection with both hint fields
/// still absent, so the recipient — which recomputes it from its own local
/// built-in — derives the same value without needing to know the publisher's
/// slug. A hash that covered the slug would be self-referential and could
/// never match across installs.
fn member_projection_with_reuse_hint(record: &AgentDefinition) -> TeamCatalogMember {
    let mut member = member_projection(record);
    if record.is_builtin {
        if let Some(slug) = record.source_team_persona_slug.clone() {
            member.projection_hash = Some(member_projection_hash(&member));
            member.builtin_slug = Some(slug);
        }
    }
    member
}

/// Canonical JSON encoding of a content body — the single serializer.
///
/// Every byte-sensitive consumer (the size contract, the content hash, and the
/// event body) routes through this function so they can never disagree about
/// what the canonical encoding is.
pub fn team_catalog_content_json(content: &TeamCatalogContent) -> Result<String, String> {
    serde_json::to_string(content).map_err(|e| format!("failed to serialize team catalog: {e}"))
}

fn member_projection_hash(member: &TeamCatalogMember) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(member).unwrap_or_default();
    hex::encode(Sha256::digest(&json))
}

fn bounded(value: &str, max: usize, label: &str) -> Result<(), String> {
    if value.len() > max {
        return Err(format!(
            "team too large to share: {label} is {} bytes (limit {max})",
            value.len()
        ));
    }
    Ok(())
}

fn validate_member(member: &TeamCatalogMember) -> Result<(), String> {
    let who = &member.display_name;
    bounded(
        &member.display_name,
        MAX_NAME_BYTES,
        "a member display name",
    )?;
    if let Some(prompt) = &member.system_prompt {
        bounded(
            prompt,
            MAX_SYSTEM_PROMPT_BYTES,
            &format!("the system prompt for '{who}'"),
        )?;
    }
    if let Some(avatar) = &member.avatar_url {
        bounded(
            avatar,
            MAX_AVATAR_URL_BYTES,
            &format!("the avatar for '{who}'"),
        )?;
    }
    if member.name_pool.len() > MAX_NAME_POOL_ENTRIES {
        return Err(format!(
            "team too large to share: '{who}' has {} name-pool entries (limit {MAX_NAME_POOL_ENTRIES})",
            member.name_pool.len()
        ));
    }
    for name in &member.name_pool {
        bounded(
            name,
            MAX_NAME_BYTES,
            &format!("a name-pool entry for '{who}'"),
        )?;
    }
    Ok(())
}

/// Enforce the size contract on a projected body.
///
/// Field bounds are checked before the total so the error names the specific
/// oversized field; the total is the backstop that actually guarantees relay
/// acceptance, because many individually-legal members still sum past the
/// ceiling.
pub fn validate_team_catalog_content(content: &TeamCatalogContent) -> Result<(), String> {
    bounded(&content.name, MAX_NAME_BYTES, "the team name")?;
    if let Some(description) = &content.description {
        bounded(description, MAX_TEXT_BYTES, "the team description")?;
    }
    if let Some(instructions) = &content.instructions {
        bounded(instructions, MAX_TEXT_BYTES, "the team instructions")?;
    }
    if content.members.len() > MAX_MEMBERS {
        return Err(format!(
            "team too large to share: {} members (limit {MAX_MEMBERS})",
            content.members.len()
        ));
    }
    for member in &content.members {
        validate_member(member)?;
    }
    let encoded = team_catalog_content_json(content)?;
    if encoded.len() > MAX_TOTAL_BYTES {
        return Err(format!(
            "team too large to share: the projection is {} bytes (limit {MAX_TOTAL_BYTES})",
            encoded.len()
        ));
    }
    Ok(())
}

/// Project a team and its resolved members onto a validated 30178 body.
///
/// `members` are supplied already resolved and ordered by the caller — the
/// team's own `persona_ids` order — because resolution needs the persona store
/// and this module stays pure. A member the caller could not resolve is simply
/// absent from the slice; the projection describes what is actually there.
///
/// Returns `Err` when the size contract is violated, so a share attempt fails
/// synchronously with a deterministic reason instead of enqueuing an event the
/// relay will refuse.
pub fn build_team_catalog_content(
    team: &TeamRecord,
    members: &[AgentDefinition],
) -> Result<TeamCatalogContent, String> {
    let content = TeamCatalogContent {
        v: TEAM_CATALOG_SCHEMA_VERSION,
        name: team.name.clone(),
        description: team.description.clone(),
        instructions: team.instructions.clone(),
        members: members
            .iter()
            .map(member_projection_with_reuse_hint)
            .collect(),
    };
    validate_team_catalog_content(&content)?;
    Ok(content)
}

/// Build an unsigned kind:30178 event for a team catalog projection.
///
/// The `d` tag is the team's id, matching its kind:30176 coordinate, so the
/// two heads for one team address consistently. `shared` is tagged only when
/// true: the relay's read gate keys off the tag's presence
/// (`SHARED_GATED_KINDS`), and an untagged head is the durable "published but
/// not discoverable" state that unshare produces.
///
/// Returns an `EventBuilder`; the caller sets `created_at`, signs, and submits.
pub fn build_team_catalog_event(
    team: &TeamRecord,
    members: &[AgentDefinition],
    shared: bool,
) -> Result<EventBuilder, String> {
    let content = build_team_catalog_content(team, members)?;
    let content_json = team_catalog_content_json(&content)?;
    let mut tags =
        vec![Tag::parse(["d", team.id.as_str()]).map_err(|e| format!("invalid d-tag: {e}"))?];
    if shared {
        tags.push(Tag::parse(["shared", "true"]).map_err(|e| format!("invalid shared tag: {e}"))?);
    }
    Ok(EventBuilder::new(Kind::Custom(KIND_TEAM_CATALOG as u16), content_json).tags(tags))
}

/// Build an unsigned kind:30178 event that republishes `content_json`
/// verbatim without the `shared` tag.
///
/// Retraction re-signs the retained body rather than reprojecting the team,
/// because retraction is triggered exactly when reprojection is impossible —
/// a member was deleted, or the team outgrew the size contract. The retained
/// body was already built and accepted at its own bounds, so replaying it
/// cannot fail the checks a fresh projection would. The team stays published
/// and readable by its author; only its discoverability is withdrawn.
pub fn build_team_catalog_retraction(
    d_tag: &str,
    content_json: &str,
) -> Result<EventBuilder, String> {
    let tag = Tag::parse(["d", d_tag]).map_err(|e| format!("invalid d-tag: {e}"))?;
    Ok(EventBuilder::new(Kind::Custom(KIND_TEAM_CATALOG as u16), content_json).tags(vec![tag]))
}

/// Parse a kind:30178 event body, rejecting an unrecognized schema version.
///
/// Version dispatch happens before field access: a future `v: 2` body may
/// legally reshape any field, so parsing it as `v: 1` and rendering whatever
/// deserializes would present a corrupted team as a valid one.
pub fn team_catalog_content_from_event(event: &nostr::Event) -> Result<TeamCatalogContent, String> {
    let content: TeamCatalogContent = serde_json::from_str(event.content.as_ref())
        .map_err(|e| format!("failed to parse team catalog content: {e}"))?;
    if content.v != TEAM_CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "unsupported team catalog schema version {} (expected {TEAM_CATALOG_SCHEMA_VERSION})",
            content.v
        ));
    }
    validate_team_catalog_content(&content)?;
    Ok(content)
}

/// Build a NIP-09 deletion (kind:5) targeting a team's kind:30178 projection.
///
/// Mirrors `team_events::build_team_delete` but at the 30178 coordinate: a
/// single `a`-tag and no `e`-tag, because an `e`-tag routes the relay to the
/// event-id deletion path and leaves the replaceable coordinate live. Deleting
/// a shared team must retract the catalog entry for every reader, not just
/// this client.
pub fn build_team_catalog_delete(
    d_tag: &str,
    owner_pubkey_hex: &str,
) -> Result<EventBuilder, String> {
    let coord = format!("{KIND_TEAM_CATALOG}:{owner_pubkey_hex}:{d_tag}");
    let tag = Tag::parse(["a", coord.as_str()]).map_err(|e| format!("invalid a-tag: {e}"))?;
    Ok(EventBuilder::new(Kind::Custom(5), "").tags(vec![tag]))
}

#[cfg(test)]
mod tests;
