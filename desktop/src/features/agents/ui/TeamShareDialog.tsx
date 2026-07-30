import * as React from "react";
import { BookUser } from "lucide-react";

import type { CatalogShareLevel } from "@/features/agents/lib/catalogRelay";
import { encodeTeamSnapshotForSend } from "@/shared/api/tauriTeams";
import type { AgentTeam } from "@/shared/api/types";

import { SnapshotShareDialog } from "./PersonaShareDialog";
import { SnapshotOptionMenu } from "./SnapshotOptionMenu";
import { teamCatalogCopy } from "./teamLibraryCopy";

type TeamShareDialogProps = {
  catalogShareLevel: CatalogShareLevel;
  isPending: boolean;
  onCatalogShareLevelChange: (shareLevel: CatalogShareLevel) => void;
  onExport: () => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  team: AgentTeam;
};

export function TeamShareDialog({
  catalogShareLevel,
  isPending,
  onCatalogShareLevelChange,
  onExport,
  onOpenChange,
  open,
  team,
}: TeamShareDialogProps) {
  const encodeSnapshot = React.useCallback(
    (memoryLevel: "none" | "core" | "everything") =>
      encodeTeamSnapshotForSend(team.id, memoryLevel, "png"),
    [team.id],
  );
  const catalogShareLevels = React.useMemo(
    () => [
      { value: "not-shared", label: teamCatalogCopy.notSharedOption },
      { value: "none", label: teamCatalogCopy.sharedOption },
    ],
    [],
  );

  return (
    <SnapshotShareDialog
      afterLink={
        // Built-in teams have no catalog head to publish — `set_team_shared`
        // rejects them backend-side, so the control is absent rather than
        // present-and-failing.
        team.isBuiltin ? null : (
          <section
            className="flex min-h-16 w-full items-center gap-3"
            data-testid="team-share-catalog"
          >
            <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground">
              <BookUser className="h-4 w-4" />
            </span>
            <div className="min-w-0 flex-1">
              <h3 className="text-sm font-medium">
                {teamCatalogCopy.shareTitle}
              </h3>
              <p className="text-xs text-secondary-foreground/75">
                {teamCatalogCopy.shareDescription}
              </p>
            </div>
            <div className="flex shrink-0 items-center">
              <SnapshotOptionMenu
                ariaLabel={teamCatalogCopy.shareAriaLabel}
                disabled={isPending}
                onValueChange={(nextValue) =>
                  onCatalogShareLevelChange(nextValue as CatalogShareLevel)
                }
                options={catalogShareLevels}
                testId="team-share-catalog-access"
                value={catalogShareLevel}
              />
            </div>
          </section>
        )
      }
      displayName={team.name}
      encodeSnapshot={encodeSnapshot}
      hasMemoryOptions
      isPending={isPending}
      onExport={onExport}
      onOpenChange={onOpenChange}
      open={open}
      snapshotKind="team"
      testIdPrefix="team-share"
    />
  );
}
