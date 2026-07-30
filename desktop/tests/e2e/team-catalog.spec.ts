import { expect, test } from "@playwright/test";

import type { RelayEvent } from "@/shared/api/types";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

type CatalogMember = {
  memberKey: string;
  displayName: string;
  systemPrompt: string;
};

/** A kind:30178 head, shaped exactly as `team_catalog_content` projects it. */
function createTeamCatalogEvent(input: {
  ownerPubkey: string;
  teamDTag: string;
  name: string;
  description?: string | null;
  members: CatalogMember[];
  createdAt?: number;
  eventId?: string;
  shared?: boolean;
}): RelayEvent {
  return {
    id: input.eventId ?? "1".repeat(64),
    pubkey: input.ownerPubkey,
    created_at: input.createdAt ?? 1_721_750_400,
    kind: 30178,
    tags: [
      ["d", input.teamDTag],
      ...(input.shared === false ? [] : [["shared", "true"]]),
    ],
    content: JSON.stringify({
      v: 1,
      name: input.name,
      description: input.description ?? null,
      instructions: null,
      members: input.members.map((member) => ({
        member_key: member.memberKey,
        display_name: member.displayName,
        system_prompt: member.systemPrompt,
        avatar_url: null,
        runtime: null,
        model: null,
      })),
    }),
    sig: "2".repeat(128),
  };
}

async function gotoAgentsView(page: import("@playwright/test").Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await expect(page.getByTestId("open-agents-view")).toBeVisible({
    timeout: 10_000,
  });
  await page.getByTestId("open-agents-view").click();
}

async function openTeamCatalog(page: import("@playwright/test").Page) {
  await page.getByTestId("new-team-card").click();
  await page.getByTestId("team-catalog-open").click();
  await expect(page.getByTestId("team-catalog-dialog")).toBeVisible();
}

async function openTeamShareDialog(
  page: import("@playwright/test").Page,
  teamName: string,
) {
  await page.getByLabel(`${teamName} team actions`).click();
  await page.getByRole("menuitem", { name: "Share" }).click();
  await expect(page.getByTestId("team-share-dialog")).toBeVisible();
}

async function setTeamCatalogAccess(
  page: import("@playwright/test").Page,
  option: "Shared" | "Not shared",
) {
  await page.getByTestId("team-share-catalog-access").click();
  await page.getByRole("menuitemradio", { exact: true, name: option }).click();
}

async function listMockTeams(page: import("@playwright/test").Page) {
  return page.evaluate(async () => {
    const invoke = (
      window as Window & {
        __BUZZ_E2E_INVOKE_MOCK_COMMAND__?: (
          command: string,
        ) => Promise<unknown>;
      }
    ).__BUZZ_E2E_INVOKE_MOCK_COMMAND__;
    if (!invoke) throw new Error("Mock invoke bridge is not installed.");
    return (await invoke("list_teams")) as Array<{
      name: string;
      persona_ids: string[];
      catalog_source: { owner_pubkey: string; team_d_tag: string } | null;
    }>;
  });
}

const ALICE_TEAM_D_TAG = "alice-review-crew";
const ALICE_TEAM_MEMBERS: CatalogMember[] = [
  {
    memberKey: "reviewer",
    displayName: "Alice’s Reviewer",
    systemPrompt: "Review changes for the whole community.",
  },
  {
    memberKey: "scribe",
    displayName: "Alice’s Scribe",
    systemPrompt: "Write the summary.",
  },
];

test("an unshared kind 30178 head from another member is not offered", async ({
  page,
}) => {
  await installMockBridge(page, {
    teamCatalogEvents: [
      createTeamCatalogEvent({
        ownerPubkey: TEST_IDENTITIES.alice.pubkey,
        teamDTag: ALICE_TEAM_D_TAG,
        name: "Alice’s Private Crew",
        members: ALICE_TEAM_MEMBERS,
        shared: false,
      }),
    ],
  });
  await gotoAgentsView(page);
  await openTeamCatalog(page);

  await expect(page.getByTestId("team-catalog-empty-state")).toBeVisible();
  await expect(
    page.locator('[data-testid^="team-catalog-list-item-"]'),
  ).toHaveCount(0);
});

test("adding another member's team records its catalog provenance", async ({
  page,
}) => {
  const entryKey = `${TEST_IDENTITIES.alice.pubkey}:${ALICE_TEAM_D_TAG}`;
  await installMockBridge(page, {
    teamCatalogEvents: [
      createTeamCatalogEvent({
        ownerPubkey: TEST_IDENTITIES.alice.pubkey,
        teamDTag: ALICE_TEAM_D_TAG,
        name: "Alice’s Review Crew",
        description: "Two agents that review and summarise.",
        members: ALICE_TEAM_MEMBERS,
      }),
    ],
  });
  await gotoAgentsView(page);
  await openTeamCatalog(page);

  const entry = page.getByTestId(`team-catalog-list-item-${entryKey}`);
  await expect(entry).toContainText("Alice’s Review Crew");
  await entry.click();

  const detail = page.getByTestId("team-catalog-detail-pane");
  await expect(detail).toContainText("Added by Community member");
  await expect(detail).toContainText("2 members");
  await expect(page.getByTestId("team-catalog-member-reviewer")).toContainText(
    "Alice’s Reviewer",
  );
  await expect(page.getByTestId("team-catalog-member-scribe")).toContainText(
    "Alice’s Scribe",
  );

  await page.getByTestId("team-catalog-add-team").click();
  await expect(
    page.getByText("Added Alice’s Review Crew to your teams."),
  ).toBeVisible();

  // The copy carries a fresh local id, so only the stored coordinate links it
  // back to Alice's publication — that link is what stops a second copy.
  const teams = await listMockTeams(page);
  const added = teams.find((team) => team.name === "Alice’s Review Crew");
  expect(added).toMatchObject({
    catalog_source: {
      owner_pubkey: TEST_IDENTITIES.alice.pubkey,
      team_d_tag: ALICE_TEAM_D_TAG,
    },
  });
  expect(added?.persona_ids).toHaveLength(2);

  await openTeamCatalog(page);
  await page.getByTestId(`team-catalog-list-item-${entryKey}`).click();
  const addButton = page.getByTestId("team-catalog-add-team");
  await expect(addButton).toHaveText("Added to my teams");
  await expect(addButton).toBeDisabled();
  expect(
    (await listMockTeams(page)).filter(
      (team) => team.name === "Alice’s Review Crew",
    ),
  ).toHaveLength(1);
});

test("a head that moved while the dialog was open is rejected", async ({
  page,
}) => {
  const entryKey = `${TEST_IDENTITIES.alice.pubkey}:${ALICE_TEAM_D_TAG}`;
  await installMockBridge(page, {
    teamCatalogEvents: [
      createTeamCatalogEvent({
        ownerPubkey: TEST_IDENTITIES.alice.pubkey,
        teamDTag: ALICE_TEAM_D_TAG,
        name: "Alice’s Review Crew",
        members: ALICE_TEAM_MEMBERS,
      }),
    ],
  });
  await gotoAgentsView(page);
  await openTeamCatalog(page);
  await page.getByTestId(`team-catalog-list-item-${entryKey}`).click();

  // Republish the coordinate without notifying subscribers: the dialog keeps
  // rendering — and keeps holding — the superseded event id.
  await page.evaluate(
    ({ ownerPubkey, teamDTag }) => {
      const replace = (
        window as Window & {
          __BUZZ_E2E_REPLACE_MOCK_TEAM_CATALOG_HEAD__?: (
            event: unknown,
          ) => void;
        }
      ).__BUZZ_E2E_REPLACE_MOCK_TEAM_CATALOG_HEAD__;
      if (!replace) throw new Error("Team catalog head seam is not installed.");
      replace({
        id: "3".repeat(64),
        pubkey: ownerPubkey,
        created_at: 1_721_760_400,
        kind: 30178,
        tags: [
          ["d", teamDTag],
          ["shared", "true"],
        ],
        content: JSON.stringify({
          v: 1,
          name: "Alice’s Review Crew",
          description: null,
          instructions: null,
          members: [],
        }),
        sig: "2".repeat(128),
      });
    },
    { ownerPubkey: TEST_IDENTITIES.alice.pubkey, teamDTag: ALICE_TEAM_D_TAG },
  );

  await page.getByTestId("team-catalog-add-team").click();
  await expect(
    page.getByText(
      "This team was updated since you opened the catalog. Reopen it and try again.",
    ),
  ).toBeVisible();
  expect(
    (await listMockTeams(page)).filter(
      (team) =>
        team.catalog_source !== null && team.catalog_source !== undefined,
    ),
  ).toHaveLength(0);
});

test("sharing a team publishes it to the catalog and unsharing retracts it", async ({
  page,
}) => {
  await installMockBridge(page, {
    personas: [
      {
        id: "custom:release-analyst",
        displayName: "Release Analyst",
        systemPrompt: "Summarise the release.",
      },
    ],
    teams: [
      {
        id: "team-release-010",
        name: "Release Crew",
        description: "Ships the release notes.",
        personaIds: ["custom:release-analyst"],
      },
    ],
  });
  await gotoAgentsView(page);

  await openTeamCatalog(page);
  await expect(page.getByTestId("team-catalog-empty-state")).toBeVisible();
  await page.keyboard.press("Escape");

  await openTeamShareDialog(page, "Release Crew");
  const catalogAccess = page.getByTestId("team-share-catalog-access");
  await expect(catalogAccess).toHaveText("Not shared");
  await setTeamCatalogAccess(page, "Shared");
  await expect(catalogAccess).toHaveText("Shared");
  await expect(
    page.getByText("Published Release Crew to the community catalog."),
  ).toBeVisible();
  await page
    .getByTestId("team-share-dialog")
    .getByRole("button", { name: "Close" })
    .click();

  await openTeamCatalog(page);
  const ownEntry = page.locator('[data-testid^="team-catalog-list-item-"]');
  await expect(ownEntry).toHaveCount(1);
  await ownEntry.click();
  await expect(page.getByTestId("team-catalog-detail-pane")).toContainText(
    "Added by You",
  );
  // The publisher already has the team, so the catalog must not offer a copy.
  await expect(page.getByTestId("team-catalog-add-team")).toBeDisabled();
  await page.keyboard.press("Escape");

  await openTeamShareDialog(page, "Release Crew");
  await setTeamCatalogAccess(page, "Not shared");
  await expect(
    page.getByText(
      "Release Crew is no longer discoverable in the community catalog.",
    ),
  ).toBeVisible();
  await page
    .getByTestId("team-share-dialog")
    .getByRole("button", { name: "Close" })
    .click();

  await openTeamCatalog(page);
  await expect(page.getByTestId("team-catalog-empty-state")).toBeVisible();
});

test("a queued team share is not presented as relay-published", async ({
  page,
}) => {
  await installMockBridge(page, {
    personas: [
      {
        id: "custom:queued-analyst",
        displayName: "Queued Analyst",
        systemPrompt: "Wait for relay acceptance.",
      },
    ],
    teams: [
      {
        id: "team-queued-011",
        name: "Queued Crew",
        description: null,
        personaIds: ["custom:queued-analyst"],
      },
    ],
    teamSharePublicationStatuses: ["queued"],
  });
  await gotoAgentsView(page);

  await openTeamShareDialog(page, "Queued Crew");
  await setTeamCatalogAccess(page, "Shared");
  await expect(
    page.getByText(
      "Sharing Queued Crew is queued. It will appear after the relay accepts the update.",
    ),
  ).toBeVisible();
  await page
    .getByTestId("team-share-dialog")
    .getByRole("button", { name: "Close" })
    .click();

  await openTeamCatalog(page);
  await expect(page.getByTestId("team-catalog-empty-state")).toBeVisible();
});
