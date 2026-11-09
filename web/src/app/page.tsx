"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { Plus } from "lucide-react";

import {
  ApiError,
  createTask,
  getHealth,
  listProfiles,
  listProjectFolders,
} from "@/lib/api";
import type { Health, LlmProfile, ProjectFolder } from "@/lib/types";
import {
  PROJECT_FOLDER_UNSET,
  SWARM_WORKERS_OPTIONS,
  TOPOLOGY_OPTIONS,
  agentsMdDefaultOn,
  buildCreateTaskInput,
  defaultProjectFolderPath,
  describeApiError,
  parseTopology,
  serviceState,
} from "@/lib/ui";
import {
  DhiBrandStatus,
  type BrandActivity,
} from "@/components/dhi-brand-status";
import { PendingTasks } from "@/components/pending-tasks";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
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
  const [projectFolders, setProjectFolders] = useState<
    ProjectFolder[] | null
  >(null);
  const [projectFoldersError, setProjectFoldersError] = useState<
    string | null
  >(null);
  const [projectFoldersUnavailable, setProjectFoldersUnavailable] =
    useState(false);
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

    listProjectFolders()
      .then((value) => {
        if (alive) {
          setProjectFolders(value);
          setProjectFoldersError(null);
          setProjectFoldersUnavailable(false);
        }
      })
      .catch((error) => {
        if (alive) {
          // A missing route means an older server: show a clean legacy-mode
          // notice instead of the generic "Not found" error prefix.
          if (error instanceof ApiError && error.isNotFound()) {
            setProjectFoldersUnavailable(true);
            setProjectFoldersError(null);
          } else {
            setProjectFoldersUnavailable(false);
            setProjectFoldersError(describeApiError(error));
          }
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
            projectFolders={projectFolders}
            projectFoldersError={projectFoldersError}
            projectFoldersUnavailable={projectFoldersUnavailable}
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
  projectFolders: ProjectFolder[] | null;
  projectFoldersError: string | null;
  projectFoldersUnavailable: boolean;
  onCreated: (task: { id: string }) => void;
  onActivity: (activity: BrandActivity) => void;
}) {
  const {
    profiles,
    profilesError,
    projectFolders,
    projectFoldersError,
    projectFoldersUnavailable,
    onCreated,
    onActivity,
  } = props;

  const [goal, setGoal] = useState("");
  const [plannerPin, setPlannerPin] = useState("");
  const [workerPin, setWorkerPin] = useState("");
  const [topology, setTopology] = useState("");
  const [swarmWorkers, setSwarmWorkers] = useState("");
  const [projectFolder, setProjectFolder] = useState(PROJECT_FOLDER_UNSET);
  const [loadAgentsMd, setLoadAgentsMd] = useState(false);
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

  // A single available folder is preselected; several require an explicit
  // choice, and a failed/empty listing stays on legacy workspace mode.
  useEffect(() => {
    if (projectFolders === null) {
      return;
    }
    const path = defaultProjectFolderPath(projectFolders);
    setProjectFolder(path);
    setLoadAgentsMd(
      agentsMdDefaultOn(projectFolders.find((folder) => folder.path === path)),
    );
  }, [projectFolders]);

  const selectedFolder =
    projectFolders?.find((folder) => folder.path === projectFolder) ?? null;

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
      projectFolder,
      loadAgentsMd,
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

          <div className="space-y-1.5">
            <label
              className="type-label font-semibold text-foreground"
              htmlFor="create-project-folder"
            >
              Project folder
            </label>

            {projectFolders === null &&
            projectFoldersError === null &&
            !projectFoldersUnavailable ? (
              <p className="type-meta text-muted-foreground">
                Loading project folders…
              </p>
            ) : null}

            {projectFoldersUnavailable ? (
              <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
                Project folders are not available on this server. Tasks still
                create in the legacy workspace — the form is not blocked.
              </p>
            ) : null}

            {projectFoldersError !== null ? (
              <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
                Could not load project folders ({projectFoldersError}). Tasks
                still create in the legacy workspace — the form is not blocked.
              </p>
            ) : null}

            {projectFolders !== null && projectFolders.length === 0 ? (
              <p className="rounded-md border border-border bg-surface-subtle px-2.5 py-1.5 type-meta text-muted-foreground">
                No project folders configured — tasks use the legacy
                workspace.
              </p>
            ) : null}

            {projectFolders !== null && projectFolders.length > 0 ? (
              <>
                <select
                  id="create-project-folder"
                  className="w-full rounded-md border-input bg-card type-body"
                  value={projectFolder}
                  onChange={(event) => {
                    const next = event.target.value;
                    setProjectFolder(next);
                    const folder =
                      projectFolders.find((entry) => entry.path === next) ??
                      null;
                    setLoadAgentsMd(agentsMdDefaultOn(folder));
                    if (formError !== null) {
                      setFormError(null);
                    }
                  }}
                >
                  <option value={PROJECT_FOLDER_UNSET}>
                    Legacy workspace (no folder)
                  </option>
                  {projectFolders.map((folder) => (
                    <option key={folder.path} value={folder.path}>
                      {folder.name} — {folder.path}
                    </option>
                  ))}
                </select>

                {selectedFolder !== null ? (
                  <>
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
                      <span
                        className="min-w-0 break-all type-mono"
                        title={selectedFolder.path}
                      >
                        {selectedFolder.path}
                      </span>
                      <Badge
                        variant={
                          selectedFolder.agents_md_present ? "success" : "muted"
                        }
                      >
                        {selectedFolder.agents_md_present
                          ? "AGENTS.md present"
                          : "No AGENTS.md"}
                      </Badge>
                    </div>

                    <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 pt-0.5">
                      <label className="flex cursor-pointer items-center gap-2 type-table text-foreground">
                        <Checkbox
                          checked={loadAgentsMd}
                          disabled={!selectedFolder.agents_md_present}
                          onCheckedChange={(checked) =>
                            setLoadAgentsMd(checked === true)
                          }
                          aria-describedby="create-agents-md-note"
                        />
                        Load AGENTS.md
                      </label>
                      {selectedFolder.agents_md_present ? (
                        <span
                          id="create-agents-md-note"
                          className="type-meta text-muted-foreground"
                        >
                          Planner-only guidance from the project folder.
                        </span>
                      ) : (
                        <span
                          id="create-agents-md-note"
                          className="type-meta text-muted-foreground/80"
                        >
                          No AGENTS.md found
                        </span>
                      )}
                    </div>
                  </>
                ) : (
                  <p className="type-meta text-muted-foreground">
                    No project folder selected — the task uses the legacy
                    workspace.
                  </p>
                )}
              </>
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

          {parseTopology(topology) === "swarm" ? (
            <div className="space-y-1.5">
              <label
                className="type-label font-semibold text-foreground"
                htmlFor="create-swarm-workers"
              >
                Workers
              </label>
              <select
                id="create-swarm-workers"
                className="w-full rounded-md border-input bg-card type-body"
                value={swarmWorkers}
                onChange={(event) => {
                  setSwarmWorkers(event.target.value);
                  if (formError !== null) {
                    setFormError(null);
                  }
                }}
              >
                {SWARM_WORKERS_OPTIONS.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
              <p className="type-meta text-muted-foreground">
                Auto lets the planner size the swarm; 2–8 pins the worker
                count.
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
  );
}
