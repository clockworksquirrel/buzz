import * as React from "react";

import type { CatalogTeam } from "@/features/agents/lib/teamCatalogRelay";
import { ProfileAvatar } from "@/features/profile/ui/ProfileAvatar";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import { ChooserDialogContent } from "@/shared/ui/chooser-dialog-content";
import { Dialog } from "@/shared/ui/dialog";
import { Skeleton } from "@/shared/ui/skeleton";

import agentOutlineUrl from "../assets/agent-outline.svg";
import { PersonaAddedBy } from "./PersonaAddedBy";
import { teamCatalogCopy } from "./teamLibraryCopy";

type TeamCatalogDialogProps = {
  error: Error | null;
  isAdding: boolean;
  isLoading: boolean;
  onAddTeam: (team: CatalogTeam) => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  teams: CatalogTeam[];
};

/** Stable per-entry key. Two owners may publish the same `d`-tag, so the
 *  coordinate — not the tag alone — is what identifies an entry. */
function catalogTeamKey(team: CatalogTeam): string {
  return `${team.ownerPubkey}:${team.teamDTag}`;
}

export function TeamCatalogDialog({
  error,
  isAdding,
  isLoading,
  onAddTeam,
  onOpenChange,
  open,
  teams,
}: TeamCatalogDialogProps) {
  const contentRef = React.useRef<HTMLDivElement | null>(null);
  const [selectedKey, setSelectedKey] = React.useState<string | null>(null);
  const selectedTeam =
    teams.find((team) => catalogTeamKey(team) === selectedKey) ??
    teams[0] ??
    null;

  // Keep the selection valid as the catalog live-updates underneath: an entry
  // that gets retracted while the dialog is open must not leave a detail pane
  // pinned to a team no longer offered.
  React.useEffect(() => {
    if (!open) return;
    setSelectedKey((current) =>
      current && teams.some((team) => catalogTeamKey(team) === current)
        ? current
        : teams[0]
          ? catalogTeamKey(teams[0])
          : null,
    );
  }, [open, teams]);

  const isSelectedTeamAdded = selectedTeam?.localTeam != null;

  return (
    <Dialog onOpenChange={onOpenChange} open={open}>
      <ChooserDialogContent
        className="h-[42rem] max-w-4xl"
        contentClassName="flex min-h-0 min-w-0 flex-1 p-0"
        data-testid="team-catalog-dialog"
        description={teamCatalogCopy.dialogDescription}
        headerClassName="bg-sidebar pb-3 text-sidebar-foreground"
        headerTestId="team-catalog-dialog-header"
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          contentRef.current?.focus();
        }}
        ref={contentRef}
        scrollAreaClassName="flex min-h-0 overflow-hidden px-0"
        scrollAreaTestId="team-catalog-dialog-body"
        tabIndex={-1}
        title={teamCatalogCopy.dialogTitle}
      >
        <TeamCatalogChooser
          error={error}
          isAdding={isAdding}
          isLoading={isLoading}
          isSelectedTeamAdded={isSelectedTeamAdded}
          onAddTeam={() => {
            if (selectedTeam && !isSelectedTeamAdded) onAddTeam(selectedTeam);
          }}
          onSelectTeam={setSelectedKey}
          selectedKey={selectedTeam ? catalogTeamKey(selectedTeam) : null}
          selectedTeam={selectedTeam}
          teams={teams}
        />
      </ChooserDialogContent>
    </Dialog>
  );
}

type TeamCatalogChooserProps = {
  error: Error | null;
  isAdding: boolean;
  isLoading: boolean;
  isSelectedTeamAdded: boolean;
  onAddTeam: () => void;
  onSelectTeam: (key: string) => void;
  selectedKey: string | null;
  selectedTeam: CatalogTeam | null;
  teams: CatalogTeam[];
};

function TeamCatalogChooser({
  error,
  isAdding,
  isLoading,
  isSelectedTeamAdded,
  onAddTeam,
  onSelectTeam,
  selectedKey,
  selectedTeam,
  teams,
}: TeamCatalogChooserProps) {
  if (!isLoading && teams.length === 0 && !error) {
    return (
      <div
        className="flex min-h-0 flex-1 items-center justify-center bg-sidebar px-6 py-10 text-center"
        data-testid="team-catalog-empty-state"
      >
        <div className="flex max-w-sm flex-col items-center">
          <img
            alt=""
            aria-hidden="true"
            className="h-32 w-32 dark:opacity-60"
            src={agentOutlineUrl}
          />
          <p className="mt-4 text-sm font-semibold">
            {teamCatalogCopy.emptyCatalogTitle}
          </p>
          <p className="mt-2 text-sm text-muted-foreground">
            {teamCatalogCopy.emptyCatalogDescription}
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-sidebar sm:flex-row">
      <div className="flex max-h-56 min-h-0 flex-col sm:max-h-none sm:w-56">
        <div
          className="min-h-0 flex-1 overflow-y-auto px-2 py-3"
          data-testid="team-catalog-dialog-scroll-area"
        >
          {isLoading ? <TeamCatalogListSkeleton /> : null}

          {!isLoading && teams.length > 0 ? (
            <div className="space-y-1">
              {teams.map((team) => {
                const key = catalogTeamKey(team);
                const isCurrent = key === selectedKey;

                return (
                  <button
                    aria-current={isCurrent ? "true" : undefined}
                    className={cn(
                      "flex w-full items-center gap-2 rounded-lg px-4 py-1.5 text-left transition-[background-color,color,box-shadow] focus:outline-hidden focus-visible:ring-2 focus-visible:ring-sidebar-ring/50 focus-visible:ring-offset-2 focus-visible:ring-offset-sidebar",
                      isCurrent
                        ? "bg-sidebar-active text-sidebar-active-foreground"
                        : "text-sidebar-foreground/70 hover:bg-sidebar-accent hover:text-sidebar-accent-foreground",
                    )}
                    data-testid={`team-catalog-list-item-${key}`}
                    key={key}
                    onClick={() => onSelectTeam(key)}
                    type="button"
                  >
                    <span className="min-w-0 flex-1 truncate text-sm font-medium">
                      {team.name}
                    </span>
                    <span className="shrink-0 text-xs tabular-nums opacity-70">
                      {team.members.length}
                    </span>
                  </button>
                );
              })}
            </div>
          ) : null}
        </div>
      </div>

      <div className="relative z-10 mb-3 ml-px mr-3 flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded-xl bg-background shadow-[-1px_0_0_0_hsl(var(--sidebar-border)/0.45)]">
        <div
          className="min-h-0 min-w-0 max-w-full flex-1 overflow-x-hidden overflow-y-auto px-5 pb-24 pt-5"
          data-testid="team-catalog-detail-pane"
        >
          {isLoading ? <TeamCatalogDetailSkeleton /> : null}

          {!isLoading && selectedTeam ? (
            <TeamCatalogDetail team={selectedTeam} />
          ) : null}

          {error ? (
            <p className="rounded-lg border border-destructive/30 bg-destructive/10 px-4 py-3 text-sm text-destructive">
              {error.message}
            </p>
          ) : null}
        </div>

        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-end bg-gradient-to-t from-background via-background/95 to-transparent px-5 pb-4 pt-12">
          <Button
            aria-label={
              selectedTeam && isSelectedTeamAdded
                ? `${selectedTeam.name} is already in your teams`
                : selectedTeam
                  ? `Add ${selectedTeam.name} from Team Catalog`
                  : undefined
            }
            className="pointer-events-auto"
            data-testid="team-catalog-add-team"
            disabled={!selectedTeam || isSelectedTeamAdded || isAdding}
            onClick={onAddTeam}
            size="sm"
            type="button"
          >
            {isSelectedTeamAdded
              ? teamCatalogCopy.addedAction
              : isAdding
                ? teamCatalogCopy.addingAction
                : teamCatalogCopy.addAction}
          </Button>
        </div>
      </div>
    </div>
  );
}

function TeamCatalogDetail({ team }: { team: CatalogTeam }) {
  return (
    <div className="w-full min-w-0 max-w-full space-y-6 overflow-x-hidden">
      <div className="min-w-0">
        <h3 className="truncate text-xl font-semibold leading-snug">
          {team.name}
        </h3>
        <PersonaAddedBy
          className="mt-0.5"
          label={team.isOwn ? "You" : "Community member"}
        />
        {team.description ? (
          <p className="mt-3 text-sm text-muted-foreground">
            {team.description}
          </p>
        ) : null}
      </div>

      <div className="min-w-0">
        <p className="text-base font-semibold text-foreground">
          {team.members.length}{" "}
          {team.members.length === 1 ? "member" : "members"}
        </p>
        <ul className="mt-3 space-y-2">
          {team.members.map((member) => (
            <li
              className="flex items-center gap-3 rounded-lg border border-border/70 bg-card/70 px-3 py-2"
              data-testid={`team-catalog-member-${member.memberKey}`}
              key={member.memberKey}
            >
              <ProfileAvatar
                avatarUrl={member.avatarUrl}
                className="h-8 w-8 text-3xs"
                label={member.displayName}
              />
              <div className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium">
                  {member.displayName}
                </p>
                <p className="truncate text-xs text-muted-foreground">
                  {member.model ?? "Use app default"}
                </p>
              </div>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

function TeamCatalogListSkeleton() {
  return (
    <div className="space-y-2">
      {["first", "second", "third", "fourth", "fifth"].map((key) => (
        <div
          className="flex items-center gap-2 rounded-lg px-4 py-1.5"
          key={key}
        >
          <Skeleton className="h-4 w-32" />
        </div>
      ))}
    </div>
  );
}

function TeamCatalogDetailSkeleton() {
  return (
    <div className="space-y-6">
      <Skeleton className="h-6 w-40" />
      <div className="space-y-2">
        <Skeleton className="h-12 rounded-lg" />
        <Skeleton className="h-12 rounded-lg" />
        <Skeleton className="h-12 rounded-lg" />
      </div>
    </div>
  );
}
