import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { relayClient } from "@/shared/api/relayClient";
import { emojiAvatarDataUrl } from "@/features/profile/ui/ProfileAvatarEditor.utils.ts";
import {
  catalogTeamsFromPublications,
  fetchTeamCatalogPublications,
  parseTeamCatalogContent,
  teamCatalogPublicationsFromEvents,
} from "./teamCatalogRelay.ts";
import { teamCatalogCopy } from "../ui/teamLibraryCopy.ts";

const ALICE = "a".repeat(64);
const BOB = "b".repeat(64);

function member(overrides = {}) {
  return {
    member_key: "reviewer",
    display_name: "Relay Reviewer",
    system_prompt: "Review changes.",
    avatar_url: null,
    runtime: "goose",
    model: "claude",
    ...overrides,
  };
}

function teamEvent({
  createdAt = 1,
  id = "alice-team",
  owner = ALICE,
  teamDTag = "squad",
  kind = 30178,
  shared = true,
  sharedTag,
  members = [member()],
  content,
  version = 1,
  name = "Review Squad",
}) {
  return {
    id,
    pubkey: owner,
    created_at: createdAt,
    kind,
    tags: [
      ["d", teamDTag],
      ...(shared
        ? [sharedTag ?? ["shared", "true"]]
        : sharedTag
          ? [sharedTag]
          : []),
    ],
    content:
      content ??
      JSON.stringify({
        v: version,
        name,
        description: "Reviews everything.",
        instructions: "Be thorough.",
        members,
      }),
    sig: "sig",
  };
}

function localTeam(overrides = {}) {
  return {
    id: "local-1",
    name: "Review Squad",
    description: null,
    instructions: null,
    personaIds: [],
    isBuiltin: false,
    shared: false,
    catalogSource: null,
    sourceDir: null,
    isSymlink: false,
    symlinkTarget: null,
    version: null,
    createdAt: "2026-07-30T00:00:00.000Z",
    updatedAt: "2026-07-30T00:00:00.000Z",
    ...overrides,
  };
}

test("test_shared_team_projection_is_discoverable_with_its_members", () => {
  const publications = teamCatalogPublicationsFromEvents([teamEvent({})]);

  assert.equal(publications.length, 1);
  assert.equal(publications[0].name, "Review Squad");
  assert.equal(publications[0].ownerPubkey, ALICE);
  assert.equal(publications[0].teamDTag, "squad");
  assert.equal(publications[0].eventId, "alice-team");
  assert.equal(publications[0].members.length, 1);
  assert.equal(publications[0].members[0].memberKey, "reviewer");
  assert.equal(publications[0].members[0].displayName, "Relay Reviewer");
});

// A team's own wire body (30176) shares the coordinate namespace with its
// catalog projection (30178) but is not a projection — reading one as the
// other would show the community a body it never opted into publishing.
test("test_team_wire_kind_is_not_read_as_a_catalog_projection", () => {
  assert.deepEqual(
    teamCatalogPublicationsFromEvents([teamEvent({ kind: 30176 })]),
    [],
  );
});

test("test_unshared_newer_head_hides_the_older_shared_team", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ createdAt: 1, id: "shared" }),
    teamEvent({ createdAt: 2, id: "retracted", shared: false }),
  ]);

  assert.deepEqual(publications, []);
});

test("test_only_an_exact_shared_true_tag_opts_a_team_into_discovery", () => {
  for (const [index, sharedTag] of [
    ["shared"],
    ["shared", "false"],
    ["shared", "true", "extra"],
  ].entries()) {
    const event = teamEvent({
      createdAt: index + 2,
      id: `malformed-${index}`,
      shared: false,
      sharedTag,
    });
    assert.deepEqual(teamCatalogPublicationsFromEvents([event]), []);
  }

  const duplicate = teamEvent({ createdAt: 5, id: "duplicate" });
  duplicate.tags.push(["shared", "true"]);
  assert.deepEqual(teamCatalogPublicationsFromEvents([duplicate]), []);
});

// Two `d` tags name two coordinates; the relay's ingest rule rejects that
// shape, so honouring the first here would resolve a coordinate the publisher
// never claimed.
test("test_two_d_tags_make_a_head_unaddressable", () => {
  const ambiguous = teamEvent({ id: "ambiguous" });
  ambiguous.tags.push(["d", "other-squad"]);

  assert.deepEqual(teamCatalogPublicationsFromEvents([ambiguous]), []);
});

test("test_unparsable_head_does_not_resurrect_an_older_shared_team", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ createdAt: 1, id: "older-valid" }),
    teamEvent({ createdAt: 2, id: "b".repeat(64), content: "{}" }),
  ]);

  assert.deepEqual(publications, []);
});

// A future body may legally reshape any field, so rendering whatever happens
// to parse as v1 would present a corrupted team as a valid one.
test("test_unknown_schema_version_is_rejected_rather_than_best_effort_parsed", () => {
  assert.deepEqual(
    teamCatalogPublicationsFromEvents([teamEvent({ version: 2 })]),
    [],
  );
  assert.deepEqual(
    teamCatalogPublicationsFromEvents([
      teamEvent({
        content: JSON.stringify({ name: "Review Squad", members: [] }),
      }),
    ]),
    [],
    "a body with no version at all is not implicitly v1",
  );
});

// Invalid members are counted, not dropped: a team with invalid members is
// shown with a diagnostic and the Add button disabled, so the user can see
// what is wrong without losing visibility of the team.
test("test_one_invalid_member_key_counts_as_invalid_not_drop", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({
      members: [member(), member({ member_key: "", display_name: "Nameless" })],
    }),
  ]);

  assert.equal(publications.length, 1, "team is still shown");
  assert.equal(
    publications[0].invalidMemberCount,
    1,
    "one invalid member counted",
  );
  assert.equal(publications[0].members.length, 1, "only valid member rendered");
});

test("test_member_missing_a_display_name_counts_as_invalid_not_drop", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ members: [member({ display_name: "   " })] }),
  ]);
  assert.equal(publications.length, 1, "team is still shown");
  assert.equal(publications[0].invalidMemberCount, 1);
  assert.equal(publications[0].members.length, 0);
});

test("test_a_team_with_no_members_is_still_a_valid_projection", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ members: [] }),
  ]);

  assert.equal(publications.length, 1);
  assert.deepEqual(publications[0].members, []);
});

/** The avatar a member projects for `avatarUrl`, or null if dropped. */
function memberAvatarUrl(avatarUrl) {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ members: [member({ avatar_url: avatarUrl })] }),
  ]);
  return publications[0].members[0].avatarUrl;
}

// A team embeds N member avatars, so it must be held to exactly the persona
// allowlist rather than the permissive string read it started with.
test("test_member_avatars_keep_bounded_http_urls_and_drop_unsafe_schemes", () => {
  assert.equal(
    memberAvatarUrl("https://relay.example/avatar.png"),
    "https://relay.example/avatar.png",
  );
  assert.equal(memberAvatarUrl("javascript:alert(1)"), null);
  assert.equal(
    memberAvatarUrl(`data:image/svg+xml;base64,${btoa("<svg/>")}`),
    null,
  );
  assert.equal(memberAvatarUrl("data:image/png,%89PNG"), null);
});

test("test_percent_encoded_emoji_member_avatar_survives_the_catalog", () => {
  const emojiAvatar = emojiAvatarDataUrl("🐝", "#FFCC00");

  assert.equal(memberAvatarUrl(emojiAvatar), emojiAvatar);
});

test("test_oversized_inline_svg_member_avatar_is_rejected", () => {
  const withinCap = `data:image/svg+xml,${"a".repeat(8_192 - "data:image/svg+xml,".length)}`;
  assert.equal(withinCap.length, 8_192);
  assert.equal(memberAvatarUrl(withinCap), withinCap);
  assert.equal(memberAvatarUrl(`${withinCap}a`), null);
});

test("test_team_coordinates_remain_independent_across_publishers", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ id: "alice", owner: ALICE }),
    teamEvent({ id: "bob", owner: BOB }),
  ]);

  assert.equal(publications.length, 2);
});

test("test_own_publication_resolves_to_the_local_team_by_id", () => {
  const publications = teamCatalogPublicationsFromEvents([teamEvent({})]);
  const own = localTeam({ id: "squad", shared: true });

  const teams = catalogTeamsFromPublications(publications, [own], ALICE);

  assert.equal(teams[0].isOwn, true);
  assert.equal(teams[0].localTeam.id, "squad");
});

// The duplicate-add bug: a copy carries a fresh local id, so only the stored
// coordinate links it back to the publication it came from.
test("test_added_foreign_entry_resolves_to_its_local_copy", () => {
  const publications = teamCatalogPublicationsFromEvents([teamEvent({})]);
  const copy = localTeam({
    id: "a-fresh-uuid",
    catalogSource: { ownerPubkey: ALICE, teamDTag: "squad" },
  });

  const teams = catalogTeamsFromPublications(publications, [copy], BOB);

  assert.equal(teams[0].isOwn, false);
  assert.equal(teams[0].localTeam.id, "a-fresh-uuid");
});

test("test_foreign_entry_with_no_local_copy_has_no_local_team", () => {
  const publications = teamCatalogPublicationsFromEvents([teamEvent({})]);
  // A same-named local team with no provenance is a different team.
  const unrelated = localTeam({ id: "unrelated" });

  const teams = catalogTeamsFromPublications(publications, [unrelated], BOB);

  assert.equal(teams[0].localTeam, null);
});

// Provenance is per-owner: the same d-tag under a different publisher is a
// different team, so a copy of Alice's must not mask Bob's entry.
test("test_catalog_source_match_is_scoped_to_the_publishing_owner", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ id: "bob-team", owner: BOB }),
  ]);
  const copyOfAlices = localTeam({
    id: "copy-of-alices",
    catalogSource: { ownerPubkey: ALICE, teamDTag: "squad" },
  });

  const teams = catalogTeamsFromPublications(
    publications,
    [copyOfAlices],
    ALICE,
  );

  assert.equal(teams[0].localTeam, null);
});

// An own team's `d`-tag is its local id, so an id match under another
// publisher's coordinate must not read as already-added.
test("test_local_id_match_under_a_foreign_owner_is_not_a_local_copy", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ owner: BOB, teamDTag: "squad" }),
  ]);
  const sameId = localTeam({ id: "squad" });

  const teams = catalogTeamsFromPublications(publications, [sameId], ALICE);

  assert.equal(teams[0].isOwn, false);
  assert.equal(teams[0].localTeam, null);
});

test("test_identity_pubkey_case_does_not_change_ownership", () => {
  const publications = teamCatalogPublicationsFromEvents([teamEvent({})]);

  const teams = catalogTeamsFromPublications(
    publications,
    [],
    ALICE.toUpperCase(),
  );

  assert.equal(teams[0].isOwn, true);
});

test("test_catalog_entries_are_sorted_by_name", () => {
  const publications = teamCatalogPublicationsFromEvents([
    teamEvent({ id: "zed", teamDTag: "zed", name: "Zed Squad" }),
    teamEvent({ id: "ace", teamDTag: "ace", name: "Ace Squad" }),
  ]);

  const teams = catalogTeamsFromPublications(publications, [], BOB);

  assert.deepEqual(
    teams.map((team) => team.name),
    ["Ace Squad", "Zed Squad"],
  );
});

function pageOfEvents(count, startId, createdAt) {
  return Array.from({ length: count }, (_, index) =>
    teamEvent({
      createdAt: typeof createdAt === "function" ? createdAt(index) : createdAt,
      id: `event-${startId + index}`,
      teamDTag: `squad-${startId + index}`,
    }),
  );
}

function stubPagedRelay(pages) {
  const filters = [];
  mock.method(relayClient, "fetchEvents", (filter) => {
    filters.push(filter);
    return Promise.resolve(pages[filters.length - 1] ?? []);
  });
  return filters;
}

// A single limit-capped fetch drops every team past the relay's clamp, making
// those teams undiscoverable.
test("test_team_catalog_paging_requests_kind_30178_and_follows_full_pages", async (t) => {
  t.after(() => mock.restoreAll());
  const filters = stubPagedRelay([
    pageOfEvents(500, 0, (index) => 10_000 - index),
    pageOfEvents(3, 500, 9_000),
  ]);

  const publications = await fetchTeamCatalogPublications();

  assert.deepEqual(filters[0].kinds, [30178]);
  assert.equal(filters.length, 2, "a full page must be followed by another");
  assert.equal(filters[0].until, undefined, "the first page has no cursor");
  assert.equal(
    filters[1].until,
    10_000 - 499,
    "the cursor must be the oldest created_at from the previous page",
  );
  assert.equal(publications.length, 503);
});

test("test_short_first_team_page_does_not_issue_a_second_request", async (t) => {
  t.after(() => mock.restoreAll());
  const filters = stubPagedRelay([pageOfEvents(2, 0, 10_000)]);

  const publications = await fetchTeamCatalogPublications();

  assert.equal(filters.length, 1);
  assert.equal(publications.length, 2);
});

// ── parseTeamCatalogContent: v1 validation contract ───────────────────────

function contentEvent(body) {
  return {
    id: "evt1",
    pubkey: ALICE,
    created_at: 1,
    kind: 30178,
    tags: [
      ["d", "squad"],
      ["shared", "true"],
    ],
    content: JSON.stringify(body),
    sig: "sig",
  };
}

function validBody(memberOverrides = {}) {
  return {
    v: 1,
    name: "Review Squad",
    members: [
      {
        member_key: "mk1",
        display_name: "Agent One",
        system_prompt: "Do it.",
        ...memberOverrides,
      },
    ],
  };
}

test("test_valid_body_yields_zero_invalid_members", () => {
  const result = parseTeamCatalogContent(contentEvent(validBody()));
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 0);
  assert.equal(result.members.length, 1);
});

test("test_wrong_schema_version_returns_null", () => {
  const result = parseTeamCatalogContent(
    contentEvent({ ...validBody(), v: 2 }),
  );
  assert.equal(result, null);
});

test("test_member_count_cap_exceeded_returns_null", () => {
  const manyMembers = Array.from({ length: 65 }, (_, i) => ({
    member_key: `k${i}`,
    display_name: `Agent ${i}`,
  }));
  const result = parseTeamCatalogContent(
    contentEvent({ v: 1, name: "Big Team", members: manyMembers }),
  );
  assert.equal(result, null);
});

test("test_member_with_parallelism_out_of_range_counts_as_invalid", () => {
  const result = parseTeamCatalogContent(
    contentEvent(validBody({ parallelism: 999 })),
  );
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 1, "parallelism 999 fails v1");
  assert.equal(result.members.length, 0, "invalid member is not rendered");
});

test("test_parallelism_at_boundary_values_is_valid", () => {
  const r1 = parseTeamCatalogContent(
    contentEvent(validBody({ parallelism: 1 })),
  );
  assert.equal(r1?.invalidMemberCount, 0);
  const r32 = parseTeamCatalogContent(
    contentEvent(validBody({ parallelism: 32 })),
  );
  assert.equal(r32?.invalidMemberCount, 0);
});

test("test_member_with_unrecognized_respond_to_counts_as_invalid", () => {
  const result = parseTeamCatalogContent(
    contentEvent(validBody({ respond_to: "nobody" })),
  );
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 1, "unknown respond_to fails v1");
});

test("test_recognized_respond_to_values_are_valid", () => {
  for (const mode of ["all", "anyone", "allowlist", "owner_only"]) {
    const r = parseTeamCatalogContent(
      contentEvent(validBody({ respond_to: mode })),
    );
    assert.equal(
      r?.invalidMemberCount,
      0,
      `respond_to '${mode}' should be valid`,
    );
  }
});

test("test_member_missing_member_key_counts_as_invalid", () => {
  const result = parseTeamCatalogContent(
    contentEvent({ v: 1, name: "T", members: [{ display_name: "A" }] }),
  );
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 1);
});

test("test_partial_builtin_hint_counts_as_invalid", () => {
  // builtin_slug present but projection_hash absent
  const result = parseTeamCatalogContent(
    contentEvent(validBody({ builtin_slug: "fizz" })),
  );
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 1, "half-pair hint must fail");
});

test("test_complete_builtin_hint_with_valid_sha256_is_valid", () => {
  const hash = "a".repeat(64);
  const result = parseTeamCatalogContent(
    contentEvent(validBody({ builtin_slug: "fizz", projection_hash: hash })),
  );
  assert.ok(result !== null);
  assert.equal(
    result.invalidMemberCount,
    0,
    "complete hint with 64-char hex hash is valid",
  );
});

test("test_multiple_invalid_members_accumulate_count", () => {
  const body = {
    v: 1,
    name: "Mixed Team",
    members: [
      { member_key: "k1", display_name: "Good", system_prompt: "OK" },
      { member_key: "k2", display_name: "Bad", parallelism: 0 },
      { member_key: "k3", display_name: "Also Bad", respond_to: "???" },
    ],
  };
  const result = parseTeamCatalogContent(contentEvent(body));
  assert.ok(result !== null);
  assert.equal(result.invalidMemberCount, 2, "two invalid members");
  assert.equal(result.members.length, 1, "one valid member rendered");
  assert.equal(result.members[0].displayName, "Good");
});

test("test_name_too_long_returns_null", () => {
  const longName = "x".repeat(300); // 300 > 256 byte limit
  const result = parseTeamCatalogContent(
    contentEvent({ v: 1, name: longName, members: [] }),
  );
  assert.equal(result, null, "oversize name must return null");
});

test("test_description_too_long_returns_null", () => {
  const body = {
    v: 1,
    name: "T",
    description: "x".repeat(5000), // > 4*1024
    members: [],
  };
  assert.equal(parseTeamCatalogContent(contentEvent(body)), null);
});

// ── F8: share disclosure copy contract ───────────────────────────────────
// Both team instructions AND member instructions are published as plaintext.
// The copy must name both to satisfy the explicit disclosure requirement.

test("test_share_disclosure_names_team_instructions", () => {
  assert.ok(
    teamCatalogCopy.shareDescription
      .toLowerCase()
      .includes("team instructions"),
    "disclosure must mention team instructions",
  );
});

test("test_share_disclosure_names_member_instructions", () => {
  assert.ok(
    teamCatalogCopy.shareDescription.toLowerCase().includes("member"),
    "disclosure must mention member instructions",
  );
  assert.ok(
    teamCatalogCopy.shareDescription.toLowerCase().includes("instructions"),
    "disclosure must explicitly say instructions are shared",
  );
});
