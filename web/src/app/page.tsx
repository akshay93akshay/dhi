"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { FileText, FolderOpen, Plus } from "lucide-react";

import { createTask, getHealth, listProfiles } from "@/lib/api";
import type { Health, LlmProfile } from "@/lib/types";
import {
  SWARM_WORKERS_HELP_TEXT,
  SWARM_WORKERS_MAX,
  SWARM_WORKERS_MIN,
  TOPOLOGY_OPTIONS,
  buildCreateTaskInput,
  describeApiError,
  serviceState,
  showsSwarmWorkersField,
} from "@/lib/ui";
import {
  EMPTY_PROJECT_SELECTION,
  clearAgentsMdPath,
  withAgentsMdPath,
  withProjectFolder,
  type FileBrowserMode,
  type ProjectSelection,
} from "@/lib/fileBrowser";
import {
  DhiBrandStatus,
  type BrandActivity,
} from "@/components/dhi-brand-status";
import { FileBrowserDialog } from "@/components/file-browser-dialog";
import { PendingTasks } from "@/components/pending-tasks";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";

/**
 * Start page: one centered animated brand, one question, and the create-task
 * form. Operational dashboards live on their own pages; this surface stays
 * focused on starting the next task.
 */
export default function Home() {
  const router = useRouter();
  const [health, setHealth] = useState<Health | null>(null);
  const [healthError, setHealthError] = useState<string | null>(null);
  const [profiles, setProfiles] = useState<LlmProfile[] | null>(null);
  const [profilesError, setProfilesError] = useState<string | null>(null);
  const [activity, setActivity] = useState<BrandActivity>("idle");

  useEffect(() => {
    let alive = true;

    getHealth()
      .then((value) => {
        if (alive) {
          setHealth(value);
          setHealthError(null);
        }
      })
      .catch((error) => {
        if (alive) {
          setHealthError(describeApiError(error));
        }
      });

    listProfiles()
      .then((value) => {
        if (alive) {
          setProfiles(value);
          setProfilesError(null);
        }
      })
      .catch((error) => {
        if (alive) {
          setProfilesError(describeApiError(error));
        }
      });

    return () => {
      alive = false;
    };
  }, []);

  return (
    <main className="flex flex-col items-center pt-6 pb-10 sm:pt-10">
      <section
        aria-labelledby="start-heading"
        className="flex w-full max-w-[760px] flex-col items-center text-center"
      >
        <DhiBrandStatus
          state={serviceState(health, healthError)}
          hero
          interactive
          activity={activity}
        />
        <h1
          id="start-heading"
          className="mt-5 type-page font-medium tracking-[-0.03em] text-foreground"
        >
          What should DHI<sup>+</sup> work on?
        </h1>
        <div className="mt-8 w-full text-left">
          <CreateTaskCard
            profiles={profiles}
            profilesError={profilesError}
            onCreated={(task) => router.push(`/tasks/${task.id}`)}
            onActivity={setActivity}
          />
        </div>
        <div className="mt-9 w-full text-left">
          <PendingTasks />
        </div>
      </section>
    </main>
  );
}

function CreateTaskCard(props: {
  profiles: LlmProfile[] | null;
  profilesError: string | null;
  onCreated: (task: { id: string }) => void;
  onActivity: (activity: BrandActivity) => void;
}) {
  const { profiles, profilesError, onCreated, onActivity } = props;

  const [goal, setGoal] = useState("");
  const [plannerPin, setPlannerPin] = useState("");
  const [workerPin, setWorkerPin] = useState("");
  const [topology, setTopology] = useState("");
  const [swarmWorkers, setSwarmWorkers] = useState("");
  const [swarmWorkersAuto, setSwarmWorkersAuto] = useState(true);
  const [projectSelection, setProjectSelection] = useState<ProjectSelection>(
    EMPTY_PROJECT_SELECTION,
  );
  const [browserMode, setBrowserMode] = useState<FileBrowserMode | null>(null);
  const [fileBrowserUnavailable, setFileBrowserUnavailable] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const activityTimeoutRef = useRef<number | null>(null);

  const clearActivityTimeout = () => {
    if (activityTimeoutRef.current !== null) {
      window.clearTimeout(activityTimeoutRef.current);
      activityTimeoutRef.current = null;
    }
  };

  const reportActivity = (next: BrandActivity) => {
    clearActivityTimeout();
    onActivity(next);
  };

  const reportTimedActivity = (next: BrandActivity, duration: number) => {
    clearActivityTimeout();
    onActivity(next);
    activityTimeoutRef.current = window.setTimeout(() => {
      activityTimeoutRef.current = null;
      onActivity("idle");
    }, duration);
  };

  useEffect(() => {
    return () => {
      if (activityTimeoutRef.current !== null) {
        window.clearTimeout(activityTimeoutRef.current);
      }
    };
  }, []);

  const markFileBrowserUnavailable = useCallback((): void => {
    setFileBrowserUnavailable(true);
  }, []);

  const handleBrowseSelect = (path: string): void => {
    if (browserMode === "folder") {
      setProjectSelection((current) => withProjectFolder(current, path));
    } else if (browserMode === "file") {
      setProjectSelection((current) => withAgentsMdPath(current, path));
    }
    if (formError !== null) {
      setFormError(null);
    }
  };

  const handleSubmit = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    reportActivity("submit");
    setFormError(null);
    const parsed = buildCreateTaskInput({
      goal,
      plannerPin,
      workerPin,
      topology,
      swarmWorkers,
      swarmWorkersAuto,
      projectFolder: projectSelection.projectFolder,
      agentsMdPath: projectSelection.agentsMdPath,
    });
    if (!parsed.ok) {
      setFormError(parsed.error);
      reportTimedActivity("error", 2_600);
      return;
    }
    setSubmitting(true);
    try {
      const task = await createTask(parsed.input);
      reportTimedActivity("success", 1_600);
      onCreated(task);
    } catch (error) {
      setFormError(describeApiError(error));
      reportTimedActivity("error", 2_600);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <>
      <Card className="flex flex-col" id="new-task">
        <CardHeader className="border-b border-border pb-4">
          <div className="flex items-center gap-2">
            <span className="flex h-7 w-7 items-center justify-center rounded-lg bg-accent text-accent-foreground">
              <Plus className="h-4 w-4" />
            </span>
            <div>
              <CardTitle>Launch a task</CardTitle>
              <CardDescription className="mt-0.5">
                One goal, one exploration, one origin fact.
              </CardDescription>
            </div>
          </div>
        </CardHeader>
        <CardContent className="flex-1 pt-4">
          <form
            onSubmit={(event) => void handleSubmit(event)}
            className="space-y-4"
            noValidate
          >
            <div className="space-y-1.5">
              <label
                className="type-label font-semibold text-foreground"
                htmlFor="create-goal"
              >
                Goal
              </label>
              <Textarea
                id="create-goal"
                value={goal}
                required
                rows={3}
                placeholder="What should the agents achieve? e.g. mate in two"
                onFocus={() => reportActivity("focus-goal")}
                onBlur={() => reportActivity("idle")}
                onChange={(event) => {
                  setGoal(event.target.value);
                  reportActivity("typing");
                  if (formError !== null) {
                    setFormError(null);
                  }
                }}
              />
            </div>

            <div className="space-y-3">
              <div className="space-y-1.5">
                <label
                  className="type-label font-semibold text-foreground"
                  htmlFor="create-project-folder"
                >
                  Project folder
                </label>
                <div className="flex items-stretch gap-2">
                  <Input
                    id="create-project-folder"
                    className="min-w-0 flex-1 type-mono"
                    value={projectSelection.projectFolder}
                    placeholder="No folder selected"
                    readOnly
                    title={projectSelection.projectFolder}
                  />
                  <Button
                    type="button"
                    variant="outline"
                    className="shrink-0"
                    onClick={() => setBrowserMode("folder")}
                  >
                    <FolderOpen className="h-4 w-4" />
                    Browse
                  </Button>
                </div>
                <p className="type-meta text-muted-foreground">
                  Absolute folder on the DHI+ host. DHI+ works in this folder
                  directly.
                </p>
              </div>

              <div className="space-y-1.5">
                <label
                  className="type-label font-semibold text-foreground"
                  htmlFor="create-agents-md"
                >
                  AGENTS.md
                </label>
                <div className="flex flex-wrap items-stretch gap-2">
                  <Input
                    id="create-agents-md"
                    className="min-w-0 flex-1 type-mono"
                    value={projectSelection.agentsMdPath}
                    placeholder="No AGENTS.md selected"
                    readOnly
                    title={projectSelection.agentsMdPath}
                  />
                  <Button
                    type="button"
                    variant="outline"
                    className="shrink-0"
                    onClick={() => setBrowserMode("file")}
                  >
                    <FileText className="h-4 w-4" />
                    Browse
                  </Button>
                  <Button
                    type="button"
                    variant="ghost"
                    className="shrink-0"
                    onClick={() =>
                      setProjectSelection((current) =>
                        clearAgentsMdPath(current),
                      )
                    }
                    disabled={projectSelection.agentsMdPath === ""}
                  >
                    Clear
                  </Button>
                </div>
                <p className="type-meta text-muted-foreground">
                  Optional planner guidance file on the DHI+ host.
                </p>
              </div>

              {fileBrowserUnavailable ? (
                <p
                  className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning"
                  role="status"
                >
                  The host file browser is unavailable on this server (404).
                  You can still configure the rest of the form.
                </p>
              ) : null}
            </div>

            {profiles === null && profilesError === null ? (
              <p className="type-meta text-muted-foreground">
                Loading profiles…
              </p>
            ) : null}

            {profilesError !== null ? (
              <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
                Could not load profiles ({profilesError}). Pins unavailable.
              </p>
            ) : null}

            {profiles !== null && profiles.length === 0 ? (
              <p className="rounded-md border border-border bg-surface-subtle px-2.5 py-1.5 type-meta text-muted-foreground">
                No profiles seeded yet — the task uses the role pool defaults.
                Seed profiles via Settings to enable pins.
              </p>
            ) : null}

            {profiles !== null && profiles.length > 0 ? (
              <div className="grid gap-3 sm:grid-cols-2">
                <div className="space-y-1.5">
                  <label
                    className="type-label font-semibold text-foreground"
                    htmlFor="create-planner-pin"
                  >
                    Planner profile
                  </label>
                  <select
                    id="create-planner-pin"
                    className="rounded-md border-input bg-card type-body"
                    value={plannerPin}
                    onFocus={() => reportActivity("focus-planner")}
                    onBlur={() => reportActivity("idle")}
                    onChange={(event) => setPlannerPin(event.target.value)}
                  >
                    <option value="">Pool default</option>
                    {profiles.map((profile) => (
                      <option key={profile.id} value={String(profile.id)}>
                        #{profile.id} {profile.name} — {profile.model}
                      </option>
                    ))}
                  </select>
                </div>
                <div className="space-y-1.5">
                  <label
                    className="type-label font-semibold text-foreground"
                    htmlFor="create-worker-pin"
                  >
                    Worker profile
                  </label>
                  <select
                    id="create-worker-pin"
                    className="rounded-md border-input bg-card type-body"
                    value={workerPin}
                    onFocus={() => reportActivity("focus-worker")}
                    onBlur={() => reportActivity("idle")}
                    onChange={(event) => setWorkerPin(event.target.value)}
                  >
                    <option value="">Pool default</option>
                    {profiles.map((profile) => (
                      <option key={profile.id} value={String(profile.id)}>
                        #{profile.id} {profile.name} — {profile.model}
                      </option>
                    ))}
                  </select>
                </div>
              </div>
            ) : null}

            <div className="space-y-1.5">
              <label
                className="type-label font-semibold text-foreground"
                htmlFor="create-topology"
              >
                Topology
              </label>
              <select
                id="create-topology"
                className="w-full rounded-md border-input bg-card type-body"
                value={topology}
                onChange={(event) => {
                  setTopology(event.target.value);
                  if (formError !== null) {
                    setFormError(null);
                  }
                }}
              >
                {TOPOLOGY_OPTIONS.map((option) => (
                  <option key={option.label} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
              <p className="type-meta text-muted-foreground">
                Unset lets the planner choose the topology per intent.
              </p>
            </div>

            {showsSwarmWorkersField(topology) ? (
              <div className="space-y-1.5">
                <label
                  className="type-label font-semibold text-foreground"
                  htmlFor="create-swarm-workers"
                >
                  Swarm workers
                </label>
                <div className="flex flex-wrap items-center gap-3">
                  <Input
                    id="create-swarm-workers"
                    className="w-28 type-mono"
                    type="number"
                    inputMode="numeric"
                    min={SWARM_WORKERS_MIN}
                    max={SWARM_WORKERS_MAX}
                    step={1}
                    placeholder={`${SWARM_WORKERS_MIN}–${SWARM_WORKERS_MAX}`}
                    value={swarmWorkers}
                    disabled={swarmWorkersAuto}
                    aria-describedby="create-swarm-workers-help"
                    onChange={(event) => {
                      setSwarmWorkers(event.target.value);
                      if (formError !== null) {
                        setFormError(null);
                      }
                    }}
                  />
                  <label
                    className="flex cursor-pointer items-center gap-2 type-table text-foreground"
                    title="Let the coordinator decide, up to 8 without approval"
                  >
                    <Checkbox
                      checked={swarmWorkersAuto}
                      onCheckedChange={(checked) => {
                        setSwarmWorkersAuto(checked === true);
                        if (formError !== null) {
                          setFormError(null);
                        }
                      }}
                      aria-describedby="create-swarm-workers-help"
                    />
                    Auto
                  </label>
                </div>
                <p
                  id="create-swarm-workers-help"
                  className="type-meta text-muted-foreground"
                >
                  {SWARM_WORKERS_HELP_TEXT}
                </p>
              </div>
            ) : null}

            {formError !== null ? (
              <div
                className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 type-meta text-destructive"
                role="alert"
              >
                {formError}
              </div>
            ) : null}

            <div className="flex items-center gap-3 pt-0.5">
              <Button
                type="submit"
                className="flex-1"
                disabled={submitting}
                onFocus={() => reportActivity("focus-submit")}
              >
                {submitting ? "Creating…" : "Create task"}
              </Button>
            </div>
            <p className="text-center type-meta text-muted-foreground">
              Tasks run immediately. Every tool call still passes the guard.
            </p>
          </form>
        </CardContent>
      </Card>

      <FileBrowserDialog
        open={browserMode !== null}
        mode={browserMode ?? "folder"}
        initialPath={projectSelection.projectFolder}
        onOpenChange={(open) => {
          if (!open) {
            setBrowserMode(null);
          }
        }}
        onSelect={handleBrowseSelect}
        onUnavailable={markFileBrowserUnavailable}
      />
    </>
  );
}
