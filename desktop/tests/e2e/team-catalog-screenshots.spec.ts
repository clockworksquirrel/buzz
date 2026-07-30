import { expect, test } from "@playwright/test";

import type { RelayEvent } from "@/shared/api/types";

import { waitForAnimations } from "../helpers/animations";
import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

const SHOTS = "test-results/team-catalog";

type CatalogMember = {
  memberKey: string;
  displayName: string;
  systemPrompt: string;
  model?: string | null;
};

/** A kind:30178 head, shaped exactly as `team_catalog_content` projects it. */
function createTeamCatalogEvent(input: {
  ownerPubkey: string;
  teamDTag: string;
  name: string;
  description?: string | null;
  members: CatalogMember[];
  eventId: string;
}): RelayEvent {
  return {
    id: input.eventId,
    pubkey: input.ownerPubkey,
    created_at: 1_721_750_400,
    kind: 30178,
    tags: [
      ["d", input.teamDTag],
      ["shared", "true"],
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
        model: member.model ?? null,
      })),
    }),
    sig: "2".repeat(128),
  };
}

const CATALOG_EVENTS: RelayEvent[] = [
  createTeamCatalogEvent({
    eventId: "a".repeat(64),
    ownerPubkey: TEST_IDENTITIES.alice.pubkey,
    teamDTag: "release-review",
    name: "Release Review",
    description:
      "Reads the diff, drafts the release note, and files the follow-ups.",
    members: [
      {
        memberKey: "reviewer",
        displayName: "Reviewer",
        systemPrompt: "Review the diff for correctness and edge cases.",
        model: "claude-sonnet-4-5",
      },
      {
        memberKey: "scribe",
        displayName: "Scribe",
        systemPrompt: "Write the release note from the merged changes.",
      },
      {
        memberKey: "triager",
        displayName: "Triager",
        systemPrompt: "File follow-ups for anything the review deferred.",
        model: "gpt-5-codex",
      },
    ],
  }),
  createTeamCatalogEvent({
    eventId: "b".repeat(64),
    ownerPubkey: TEST_IDENTITIES.bob.pubkey,
    teamDTag: "incident-desk",
    name: "Incident Desk",
    description: "Two agents that hold the timeline during an incident.",
    members: [
      {
        memberKey: "commander",
        displayName: "Commander",
        systemPrompt: "Own the incident timeline and the comms cadence.",
      },
      {
        memberKey: "investigator",
        displayName: "Investigator",
        systemPrompt: "Chase the root cause and report findings.",
      },
    ],
  }),
];

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
  await waitForAnimations(page);
}

test.describe("team catalog screenshots", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("01 — catalog browse, add, and added states", async ({ page }) => {
    test.setTimeout(60_000);
    await installMockBridge(page, { teamCatalogEvents: CATALOG_EVENTS });
    await gotoAgentsView(page);
    await openTeamCatalog(page);

    // 1. Browsing another member's publication: list, provenance, per-member
    //    model. Release Review is not the default selection, so click it.
    const releaseReview = `team-catalog-list-item-${TEST_IDENTITIES.alice.pubkey}:release-review`;
    await page.getByTestId(releaseReview).click();
    await waitForAnimations(page);
    await page.getByTestId("team-catalog-dialog").screenshot({
      path: `${SHOTS}/catalog-browse.png`,
    });

    // 2. Adding closes the dialog and names the team in the notice.
    await page.getByTestId("team-catalog-add-team").click();
    await expect(
      page.getByText("Added Release Review to your teams."),
    ).toBeVisible();

    // 3. Reopened: the action reads "Added to my teams" and is inert, so a
    //    second copy of the same publication is not offered.
    await openTeamCatalog(page);
    await page.getByTestId(releaseReview).click();
    await expect(page.getByTestId("team-catalog-add-team")).toHaveText(
      "Added to my teams",
    );
    await waitForAnimations(page);
    await page.getByTestId("team-catalog-dialog").screenshot({
      path: `${SHOTS}/catalog-added.png`,
    });
  });

  test("02 — empty catalog", async ({ page }) => {
    await installMockBridge(page, { teamCatalogEvents: [] });
    await gotoAgentsView(page);
    await openTeamCatalog(page);

    await page.getByTestId("team-catalog-dialog").screenshot({
      path: `${SHOTS}/catalog-empty.png`,
    });
  });

  test("03 — share dialog before and after publishing", async ({ page }) => {
    test.setTimeout(60_000);
    await installMockBridge(page, {
      personas: [
        {
          id: "custom:release-analyst",
          displayName: "Release Analyst",
          systemPrompt: "Summarise the release.",
        },
        {
          id: "custom:release-scribe",
          displayName: "Release Scribe",
          systemPrompt: "Write the release note.",
        },
      ],
      teams: [
        {
          id: "team-release-010",
          name: "Release Crew",
          description: "Ships the release notes.",
          personaIds: ["custom:release-analyst", "custom:release-scribe"],
        },
      ],
    });
    await gotoAgentsView(page);

    await page.getByLabel("Release Crew team actions").click();
    await page.getByRole("menuitem", { name: "Share" }).click();
    await expect(page.getByTestId("team-share-dialog")).toBeVisible();
    await waitForAnimations(page);

    // 4. Catalog access sits alongside the existing snapshot-share controls,
    //    defaulting to "Not shared".
    await page.getByTestId("team-share-dialog").screenshot({
      path: `${SHOTS}/share-not-shared.png`,
    });

    // 5. The access menu: the two states a publisher can choose between.
    await page.getByTestId("team-share-catalog-access").click();
    await waitForAnimations(page);
    await page.screenshot({ path: `${SHOTS}/share-access-menu.png` });

    // 6. Published: the control reads "Shared" and the toast names the effect.
    await page
      .getByRole("menuitemradio", { exact: true, name: "Shared" })
      .click();
    await expect(page.getByTestId("team-share-catalog-access")).toHaveText(
      "Shared",
    );
    await expect(
      page.getByText("Published Release Crew to the community catalog."),
    ).toBeVisible();
    await waitForAnimations(page);
    await page.screenshot({ path: `${SHOTS}/share-published.png` });
  });
});
