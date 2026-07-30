export const teamCatalogCopy = {
  chooseFromCatalog: "Choose from catalog",
  dialogTitle: "Team Catalog",
  dialogDescription: "Browse teams shared to this relay.",
  emptyCatalogTitle: "No teams are being shared",
  emptyCatalogDescription: "Shared teams will appear here.",
  addAction: "Add team",
  addedAction: "Added to my teams",
  addingAction: "Adding…",
  shareTitle: "Share to catalog",
  shareDescription:
    "Anyone in this community can find and add a copy of this team. Every member’s instruction is shared as plaintext. Memories and secrets aren’t included.",
  shareAriaLabel: "What to share in the catalog",
  notSharedOption: "Not shared",
  sharedOption: "Shared",
} as const;

/**
 * The result message for a share toggle.
 *
 * `queued` is not a failure: the head is durably enqueued and the flush loop
 * will publish it, so the copy promises eventual visibility rather than
 * claiming the catalog already changed.
 */
export function teamShareNotice(
  teamName: string,
  shared: boolean,
  publicationStatus: "published" | "queued",
): string {
  if (publicationStatus === "queued") {
    return shared
      ? `Sharing ${teamName} is queued. It will appear after the relay accepts the update.`
      : `Removing ${teamName} is queued. It may remain discoverable until the relay accepts the update.`;
  }
  return shared
    ? `Published ${teamName} to the community catalog.`
    : `${teamName} is no longer discoverable in the community catalog.`;
}
