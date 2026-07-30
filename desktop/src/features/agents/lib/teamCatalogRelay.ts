import {
  fetchCatalogEvents,
  safeCatalogAvatarUrl,
  sharedCatalogHeads,
} from "@/features/agents/lib/catalogRelay";
import type { AgentTeam, RelayEvent } from "@/shared/api/types";
import { KIND_TEAM_CATALOG } from "@/shared/constants/kinds";

/**
 * Read the kind:30178 team catalog.
 *
 * The projection is self-contained by design: every member's safe definition
 * is embedded, so this module renders a published team without resolving
 * anything in the publisher's namespace. `member_key` is deliberately treated
 * as an opaque label here and never as a kind:30175 coordinate — the publisher
 * may never have shared that member individually.
 *
 * Adding is NOT done from this data. The frontend passes only the coordinate
 * to `add_team_from_catalog`, which re-fetches and re-verifies the head
 * backend-side; what is parsed here is for display and for deciding whether
 * "Add" is offered.
 */

/** Schema version this client understands. Must match the backend's. */
const TEAM_CATALOG_SCHEMA_VERSION = 1;

export type CatalogTeamMember = {
  memberKey: string;
  displayName: string;
  systemPrompt: string;
  avatarUrl: string | null;
  runtime: string | null;
  model: string | null;
};

export type CatalogTeam = {
  /** The head event this projection was built from. Passed to the backend so
   *  it can reject an add whose head moved since the dialog opened. */
  eventId: string;
  ownerPubkey: string;
  teamDTag: string;
  isOwn: boolean;
  name: string;
  description: string | null;
  instructions: string | null;
  members: CatalogTeamMember[];
  /** The local team already copied from this publication, if any. */
  localTeam: AgentTeam | null;
};

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function optionalString(value: unknown): string | null {
  return typeof value === "string" && value.trim().length > 0 ? value : null;
}

function parseMember(value: unknown): CatalogTeamMember | null {
  if (
    !isObject(value) ||
    typeof value.member_key !== "string" ||
    value.member_key.length === 0 ||
    typeof value.display_name !== "string" ||
    value.display_name.trim().length === 0
  ) {
    return null;
  }
  return {
    memberKey: value.member_key,
    displayName: value.display_name,
    systemPrompt:
      typeof value.system_prompt === "string" ? value.system_prompt : "",
    avatarUrl: safeCatalogAvatarUrl(value.avatar_url),
    runtime: optionalString(value.runtime),
    model: optionalString(value.model),
  };
}

/**
 * Parse a 30178 content body, rejecting an unrecognized schema version.
 *
 * Version dispatch happens before field access, mirroring the backend: a
 * future `v: 2` body may legally reshape any field, so rendering whatever
 * happens to parse as `v: 1` would present a corrupted team as a valid one.
 * A single unparsable member fails the whole projection rather than being
 * skipped — a team shown with a member missing is a different team than the
 * one its owner published.
 */
export function parseTeamCatalogContent(
  event: RelayEvent,
): Pick<
  CatalogTeam,
  "name" | "description" | "instructions" | "members"
> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(event.content);
  } catch {
    return null;
  }
  if (
    !isObject(parsed) ||
    parsed.v !== TEAM_CATALOG_SCHEMA_VERSION ||
    typeof parsed.name !== "string" ||
    parsed.name.trim().length === 0 ||
    !Array.isArray(parsed.members)
  ) {
    return null;
  }

  const members: CatalogTeamMember[] = [];
  for (const candidate of parsed.members) {
    const member = parseMember(candidate);
    if (!member) return null;
    members.push(member);
  }

  return {
    name: parsed.name,
    description: optionalString(parsed.description),
    instructions: optionalString(parsed.instructions),
    members,
  };
}

export type TeamCatalogPublication = Omit<CatalogTeam, "isOwn" | "localTeam">;

/**
 * Project the shared kind:30178 heads onto team publications.
 *
 * A head whose content does not parse is dropped, not retried against an older
 * event: the coordinate is already claimed, so falling back would resurrect a
 * superseded definition.
 */
export function teamCatalogPublicationsFromEvents(
  events: readonly RelayEvent[],
): TeamCatalogPublication[] {
  const publications: TeamCatalogPublication[] = [];

  for (const head of sharedCatalogHeads(events, KIND_TEAM_CATALOG)) {
    const content = parseTeamCatalogContent(head.event);
    if (!content) continue;
    publications.push({
      eventId: head.event.id,
      ownerPubkey: head.ownerPubkey,
      teamDTag: head.dTag,
      ...content,
    });
  }

  return publications;
}

/** Read every shared team event, page by page. */
export async function fetchTeamCatalogPublications(): Promise<
  TeamCatalogPublication[]
> {
  return teamCatalogPublicationsFromEvents(
    await fetchCatalogEvents(KIND_TEAM_CATALOG),
  );
}

/**
 * The local team backing a catalog entry, if the user already has it.
 *
 * An own publication is found by id — its `d`-tag *is* the local team id. A
 * copy of another owner's entry carries a fresh local id instead, so the only
 * link back is the `catalogSource` coordinate stored on the copy. Matching on
 * that coordinate is what stops the catalog from offering "Add" for an entry
 * the user already added, which would mint a second copy.
 */
export function findLocalTeamForCatalogEntry(
  localTeams: readonly AgentTeam[],
  publication: TeamCatalogPublication,
  isOwn: boolean,
): AgentTeam | null {
  if (isOwn) {
    return localTeams.find((team) => team.id === publication.teamDTag) ?? null;
  }
  return (
    localTeams.find(
      (team) =>
        team.catalogSource?.ownerPubkey === publication.ownerPubkey &&
        team.catalogSource?.teamDTag === publication.teamDTag,
    ) ?? null
  );
}

export function catalogTeamsFromPublications(
  publications: readonly TeamCatalogPublication[],
  localTeams: readonly AgentTeam[],
  currentPubkey: string | null | undefined,
): CatalogTeam[] {
  const normalizedCurrentPubkey = currentPubkey?.toLowerCase() ?? null;

  return publications
    .map((publication) => {
      const isOwn = publication.ownerPubkey === normalizedCurrentPubkey;
      return {
        ...publication,
        isOwn,
        localTeam: findLocalTeamForCatalogEntry(localTeams, publication, isOwn),
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
}
