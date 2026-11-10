"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import {
  FolderOpen,
  Pencil,
  Plus,
  RefreshCw,
  ShieldCheck,
  Square,
  Upload,
} from "lucide-react";

import {
  ApiError,
  createTask,
  createWebProject,
  getHealth,
  getTaskOutputs,
  listProfiles,
  listProjectFolders,
  listWebProjects,
  uploadWebProjectFile,
} from "@/lib/api";
import type {
  Health,
  LlmProfile,
  ProjectFolder,
  WebProject,
} from "@/lib/types";
import {
  PROJECT_FOLDER_UNSET,
  SWARM_WORKERS_HELP_TEXT,
  SWARM_WORKERS_MAX,
  SWARM_WORKERS_MIN,
  TOPOLOGY_OPTIONS,
  agentsMdDefaultOn,
  buildCreateTaskInput,
  defaultProjectSelection,
  describeApiError,
  serviceState,
  showsSwarmWorkersField,
  webProjectAgentsMdLabel,
  webProjectIdFromSelection,
  webProjectSelectValue,
} from "@/lib/ui";
import {
  FILE_SYSTEM_ACCESS_FALLBACK_TEXT,
  WEB_PROJECT_LIMITS_TEXT,
  WEB_PROJECT_SKIP_DIRECTORIES,
  ensureProjectPermission,
  errorMessage,
  formatBytes,
  isFileSystemAccessSupported,
  loadProjectHandle,
  pickProjectFolder,
  projectHandlePermission,
  readAgentsMd,
  saveProjectHandle,
  snapshotLimitError,
  syncTaskOutputs,
  uploadProjectFiles,
  walkProjectDirectory,
  writeAgentsMd,
  type OutputSyncProgress,
  type OutputSyncSummary,
  type ProjectDirectoryHandle,
  type ProjectPermissionState,
  type ProjectSnapshot,
  type SnapshotUploadProgress,
} from "@/lib/webProject";
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
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
  const [projectFolders, setProjectFolders] = useState<
    ProjectFolder[] | null
  >(null);
  const [projectFoldersError, setProjectFoldersError] = useState<
    string | null
  >(null);
  const [projectFoldersUnavailable, setProjectFoldersUnavailable] =
    useState(false);
  const [webProjects, setWebProjects] = useState<WebProject[] | null>(null);
  const [webProjectsError, setWebProjectsError] = useState<string | null>(null);
  const [webProjectsUnavailable, setWebProjectsUnavailable] = useState(false);
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
          setProjectFolders([]);
          if (error instanceof ApiError && error.isNotFound()) {
            setProjectFoldersUnavailable(true);
            setProjectFoldersError(null);
          } else {
            setProjectFoldersUnavailable(false);
            setProjectFoldersError(describeApiError(error));
          }
        }
      });

    listWebProjects()
      .then((value) => {
        if (alive) {
          setWebProjects(value);
          setWebProjectsError(null);
          setWebProjectsUnavailable(false);
        }
      })
      .catch((error) => {
        if (alive) {
          // Web projects are additive: an older server (or a failed listing)
          // must never block task creation.
          setWebProjects([]);
          if (error instanceof ApiError && error.isNotFound()) {
            setWebProjectsUnavailable(true);
            setWebProjectsError(null);
          } else {
            setWebProjectsUnavailable(false);
            setWebProjectsError(describeApiError(error));
          }
        }
      });

    return () => {
      alive = false;
    };
  }, []);

  const handleWebProjectChanged = (project: WebProject): void => {
    setWebProjects((current) => {
      const base = current ?? [];
      return base.some((entry) => entry.id === project.id)
        ? base.map((entry) => (entry.id === project.id ? project : entry))
        : [...base, project];
    });
  };

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
            webProjects={webProjects}
            webProjectsError={webProjectsError}
            webProjectsUnavailable={webProjectsUnavailable}
            onWebProjectChanged={handleWebProjectChanged}
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
  webProjects: WebProject[] | null;
  webProjectsError: string | null;
  webProjectsUnavailable: boolean;
  onWebProjectChanged: (project: WebProject) => void;
  onCreated: (task: { id: string }) => void;
  onActivity: (activity: BrandActivity) => void;
}) {
  const {
    profiles,
    profilesError,
    projectFolders,
    projectFoldersError,
    projectFoldersUnavailable,
    webProjects,
    webProjectsError,
    webProjectsUnavailable,
    onWebProjectChanged,
    onCreated,
    onActivity,
  } = props;

  const [goal, setGoal] = useState("");
  const [plannerPin, setPlannerPin] = useState("");
  const [workerPin, setWorkerPin] = useState("");
  const [topology, setTopology] = useState("");
  const [swarmWorkers, setSwarmWorkers] = useState("");
  const [swarmWorkersAuto, setSwarmWorkersAuto] = useState(true);
  const [projectSelection, setProjectSelection] = useState(
    PROJECT_FOLDER_UNSET,
  );
  const [loadAgentsMd, setLoadAgentsMd] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const activityTimeoutRef = useRef<number | null>(null);
  const projectDefaultsAppliedRef = useRef(false);

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

  // A single available project option is preselected; several require an
  // explicit choice. This runs once, when both listings first resolve, so a
  // newly uploaded project does not reset the user's selection.
  useEffect(() => {
    if (
      projectFolders === null ||
      webProjects === null ||
      projectDefaultsAppliedRef.current
    ) {
      return;
    }
    projectDefaultsAppliedRef.current = true;
    if (projectSelection !== PROJECT_FOLDER_UNSET) {
      return;
    }
    const selection = defaultProjectSelection(projectFolders, webProjects);
    setProjectSelection(selection);
    const web = webProjects.find(
      (project) => webProjectSelectValue(project.id) === selection,
    );
    const folder = projectFolders.find(
      (entry) => entry.path === selection,
    );
    setLoadAgentsMd(agentsMdDefaultOn(web ?? folder ?? null));
  }, [projectFolders, webProjects, projectSelection]);

  const selectedWebProjectId = webProjectIdFromSelection(projectSelection);
  const selectedWebProject =
    selectedWebProjectId === null
      ? null
      : (webProjects?.find(
          (project) => project.id === selectedWebProjectId,
        ) ?? null);
  const selectedFolder =
    selectedWebProjectId !== null
      ? null
      : (projectFolders?.find(
          (folder) => folder.path === projectSelection,
        ) ?? null);

  const availableProjectCount =
    (projectFolders?.length ?? 0) + (webProjects?.length ?? 0);

  const selectProject = (value: string): void => {
    setProjectSelection(value);
    const webId = webProjectIdFromSelection(value);
    const web =
      webId === null
        ? null
        : (webProjects?.find((project) => project.id === webId) ?? null);
    const folder =
      projectFolders?.find((entry) => entry.path === value) ?? null;
    setLoadAgentsMd(agentsMdDefaultOn(web ?? folder ?? null));
    if (formError !== null) {
      setFormError(null);
    }
  };

  const handleWebProjectAdded = (project: WebProject): void => {
    onWebProjectChanged(project);
    setProjectSelection(webProjectSelectValue(project.id));
    setLoadAgentsMd(project.agents_md_present);
  };

  const handleWebProjectUpdated = (project: WebProject): void => {
    onWebProjectChanged(project);
    // A completed upload or a saved AGENTS.md can turn the file on. Never
    // force it off: the user may have chosen another project in the meantime.
    if (project.agents_md_present) {
      setLoadAgentsMd(true);
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
      projectFolder: selectedWebProjectId === null ? projectSelection : "",
      webProjectId: selectedWebProjectId ?? "",
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

          <div className="space-y-1.5">
            <label
              className="type-label font-semibold text-foreground"
              htmlFor="create-project-folder"
            >
              Project
            </label>

            {projectFolders === null ? (
              <p className="type-meta text-muted-foreground">
                Loading project folders…
              </p>
            ) : null}

            {projectFolders !== null && webProjects === null ? (
              <p className="type-meta text-muted-foreground">
                Loading web folders…
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

            {webProjectsUnavailable ? (
              <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
                Web folder uploads are not available on this server. Docker
                folders and the legacy workspace still work — the form is not
                blocked.
              </p>
            ) : null}

            {webProjectsError !== null ? (
              <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
                Could not load web folders ({webProjectsError}). Uploads are
                unavailable — the form is not blocked.
              </p>
            ) : null}

            {projectFolders !== null &&
            webProjects !== null &&
            availableProjectCount === 0 ? (
              <p className="rounded-md border border-border bg-surface-subtle px-2.5 py-1.5 type-meta text-muted-foreground">
                No project folders or web folders yet — tasks use the legacy
                workspace.
              </p>
            ) : null}

            {projectFolders !== null && availableProjectCount > 0 ? (
              <>
                <select
                  id="create-project-folder"
                  className="w-full rounded-md border-input bg-card type-body"
                  value={projectSelection}
                  onChange={(event) => selectProject(event.target.value)}
                >
                  <option value={PROJECT_FOLDER_UNSET}>
                    Legacy workspace (no folder)
                  </option>
                  {projectFolders.map((folder) => (
                    <option key={folder.path} value={folder.path}>
                      {folder.name} — {folder.path}
                    </option>
                  ))}
                  {webProjects?.map((project) => (
                    <option
                      key={`web-${project.id}`}
                      value={webProjectSelectValue(project.id)}
                    >
                      Web folder: {project.name}
                    </option>
                  ))}
                </select>

                {selectedWebProject !== null ? (
                  <>
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
                      <FolderOpen className="h-3.5 w-3.5 shrink-0" />
                      <span className="font-medium text-foreground">
                        {selectedWebProject.name}
                      </span>
                      <Badge
                        variant={
                          selectedWebProject.agents_md_present
                            ? "success"
                            : "muted"
                        }
                      >
                        {webProjectAgentsMdLabel(
                          selectedWebProject.agents_md_present,
                        )}
                      </Badge>
                      {typeof selectedWebProject.file_count === "number" ? (
                        <span>· {selectedWebProject.file_count} files</span>
                      ) : null}
                      {typeof selectedWebProject.total_bytes === "number" ? (
                        <span>
                          · {formatBytes(selectedWebProject.total_bytes)}
                        </span>
                      ) : null}
                    </div>

                    <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 pt-0.5">
                      <label className="flex cursor-pointer items-center gap-2 type-table text-foreground">
                        <Checkbox
                          checked={loadAgentsMd}
                          disabled={!selectedWebProject.agents_md_present}
                          onCheckedChange={(checked) =>
                            setLoadAgentsMd(checked === true)
                          }
                          aria-describedby="create-agents-md-note"
                        />
                        Load AGENTS.md
                      </label>
                      {selectedWebProject.agents_md_present ? (
                        <span
                          id="create-agents-md-note"
                          className="type-meta text-muted-foreground"
                        >
                          Planner-only guidance from the uploaded snapshot.
                        </span>
                      ) : (
                        <span
                          id="create-agents-md-note"
                          className="type-meta text-muted-foreground/80"
                        >
                          No AGENTS.md in the uploaded snapshot
                        </span>
                      )}
                    </div>
                  </>
                ) : selectedFolder !== null ? (
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
                    No project selected — the task uses the legacy workspace.
                  </p>
                )}
              </>
            ) : null}

            {webProjects !== null &&
            !webProjectsUnavailable &&
            webProjectsError === null ? (
              <WebFolderSection
                selectedProject={selectedWebProject}
                onProjectAdded={handleWebProjectAdded}
                onProjectUpdated={handleWebProjectUpdated}
              />
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
  );
}

// ---------------------------------------------------------------------------
// Browser-folder picker, snapshot upload, and AGENTS.md editor
// ---------------------------------------------------------------------------

/**
 * The web-folder half of the project picker. The folder stays local: this
 * section walks it, uploads snapshot bytes, keeps the handle in IndexedDB,
 * and writes `AGENTS.md` back through the same handle.
 */
function WebFolderSection(props: {
  selectedProject: WebProject | null;
  onProjectAdded: (project: WebProject) => void;
  onProjectUpdated: (project: WebProject) => void;
}) {
  const { selectedProject, onProjectAdded, onProjectUpdated } = props;
  const [supported, setSupported] = useState(false);
  const [handle, setHandle] = useState<ProjectDirectoryHandle | null>(null);
  const [permission, setPermission] = useState<ProjectPermissionState | null>(
    null,
  );
  const [handleLoading, setHandleLoading] = useState(true);
  const [folderError, setFolderError] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<ProjectSnapshot | null>(null);
  const [scanning, setScanning] = useState(false);
  const [agentsPresent, setAgentsPresent] = useState<boolean | null>(null);
  const [projectName, setProjectName] = useState("");
  const [uploading, setUploading] = useState(false);
  const [progress, setProgress] = useState<SnapshotUploadProgress | null>(null);
  const [uploadError, setUploadError] = useState<string | null>(null);
  const [uploadNotice, setUploadNotice] = useState<string | null>(null);
  const [createdProject, setCreatedProject] = useState<WebProject | null>(null);
  const [editorOpen, setEditorOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const [editorLoading, setEditorLoading] = useState(false);
  const [editorSaving, setEditorSaving] = useState(false);
  const [editorError, setEditorError] = useState<string | null>(null);
  const [syncTaskId, setSyncTaskId] = useState("");
  const [syncing, setSyncing] = useState(false);
  const [syncProgress, setSyncProgress] = useState<OutputSyncProgress | null>(
    null,
  );
  const [syncSummary, setSyncSummary] = useState<OutputSyncSummary | null>(
    null,
  );
  const [syncError, setSyncError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const [handleProjectId, setHandleProjectId] = useState<string | null>(null);
  const handleOwnerRef = useRef<string | null>(null);

  const rememberHandleOwner = (projectId: string | null): void => {
    handleOwnerRef.current = projectId;
    setHandleProjectId(projectId);
  };

  async function inspectFolder(target: ProjectDirectoryHandle): Promise<void> {
    setScanning(true);
    setFolderError(null);
    try {
      const walked = await walkProjectDirectory(target);
      setSnapshot(walked);
      const agents = await readAgentsMd(target);
      setAgentsPresent(agents !== null);
    } catch (cause) {
      setFolderError(errorMessage(cause));
    } finally {
      setScanning(false);
    }
  }

  useEffect(() => {
    const supportedNow = isFileSystemAccessSupported();
    setSupported(supportedNow);
    if (!supportedNow) {
      setHandleLoading(false);
      return;
    }

    const projectId = selectedProject?.id ?? null;
    if (projectId === null) {
      setHandleLoading(false);
      return;
    }
    if (handleOwnerRef.current === projectId) {
      setHandleLoading(false);
      return;
    }

    let alive = true;
    setHandleLoading(true);
    setFolderError(null);
    // Never show or reuse another project's folder while this project's
    // handle is resolved.
    setHandle(null);
    setPermission(null);
    setSnapshot(null);
    setAgentsPresent(null);
    setCreatedProject(null);
    setUploadError(null);
    setUploadNotice(null);
    setSyncSummary(null);
    setSyncError(null);
    rememberHandleOwner(null);

    void (async () => {
      try {
        const saved = await loadProjectHandle(projectId);
        if (!alive) {
          return;
        }
        if (saved === null) {
          setProjectName("");
          return;
        }
        setHandle(saved);
        rememberHandleOwner(projectId);
        setProjectName(saved.name);
        const state = await projectHandlePermission(saved);
        if (!alive) {
          return;
        }
        setPermission(state);
        if (state === "granted") {
          await inspectFolder(saved);
        }
      } catch (cause) {
        if (alive) {
          setFolderError(errorMessage(cause));
        }
      } finally {
        if (alive) {
          setHandleLoading(false);
        }
      }
    })();
    return () => {
      alive = false;
    };
  }, [selectedProject?.id]);

  // Long uploads and result syncs only run while this tab stays open.
  useEffect(() => {
    if (!uploading && !syncing) {
      return;
    }
    const warn = (event: BeforeUnloadEvent): void => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [uploading, syncing]);

  const chooseFolder = async (): Promise<void> => {
    setFolderError(null);
    setUploadError(null);
    setUploadNotice(null);
    setSnapshot(null);
    setAgentsPresent(null);
    setCreatedProject(null);
    setProgress(null);
    rememberHandleOwner(null);
    try {
      const picked = await pickProjectFolder();
      if (picked === null) {
        return;
      }
      setHandle(picked);
      setProjectName(picked.name);
      setPermission("granted");
      await inspectFolder(picked);
    } catch (cause) {
      setFolderError(errorMessage(cause));
    }
  };

  const grantAccess = async (): Promise<void> => {
    if (handle === null) {
      return;
    }
    setFolderError(null);
    const granted = await ensureProjectPermission(handle);
    setPermission(granted ? "granted" : "denied");
    if (granted) {
      await inspectFolder(handle);
    } else {
      setFolderError("Folder access was not granted.");
    }
  };

  const uploadSnapshot = async (): Promise<void> => {
    if (handle === null) {
      setUploadError("Choose a folder first.");
      return;
    }
    setUploading(true);
    setUploadError(null);
    setUploadNotice(null);
    setProgress(null);
    try {
      if (!(await ensureProjectPermission(handle))) {
        setPermission("denied");
        setUploadError("Folder access was not granted.");
        return;
      }
      setPermission("granted");

      // Always rescan so the upload reflects the folder as it is now, not the
      // metadata captured when the folder was chosen.
      const currentSnapshot = await walkProjectDirectory(handle);
      setSnapshot(currentSnapshot);
      const limitError = snapshotLimitError(currentSnapshot);
      if (limitError !== null) {
        setUploadError(limitError);
        return;
      }

      const name = projectName.trim() === "" ? handle.name : projectName.trim();
      let project = createdProject;
      if (project === null) {
        project = await createWebProject(name);
        setCreatedProject(project);
        // Associate the folder before selecting the new project, so the
        // selection-change effect never clears this handle mid-upload.
        rememberHandleOwner(project.id);
        onProjectAdded(project);
      }
      await saveProjectHandle(project.id, handle);
      rememberHandleOwner(project.id);

      const controller = new AbortController();
      abortRef.current = controller;
      const result = await uploadProjectFiles(project.id, currentSnapshot, {
        signal: controller.signal,
        onProgress: setProgress,
      });

      const hasAgentsMd = currentSnapshot.files.some(
        (entry) => entry.path === "AGENTS.md",
      );
      const updated: WebProject = {
        ...project,
        agents_md_present: hasAgentsMd,
        file_count: result.uploaded,
        total_bytes: result.totalBytes,
      };
      setCreatedProject(updated);
      setAgentsPresent(hasAgentsMd);
      onProjectUpdated(updated);
      setUploadNotice(
        `Uploaded ${result.uploaded} file${result.uploaded === 1 ? "" : "s"} (${formatBytes(result.totalBytes)}) as “${updated.name}”.`,
      );
    } catch (cause) {
      if (cause instanceof Error && cause.name === "AbortError") {
        setUploadError("Upload cancelled. The project is kept; retry when ready.");
      } else {
        setUploadError(errorMessage(cause));
      }
    } finally {
      abortRef.current = null;
      setUploading(false);
      setProgress(null);
    }
  };

  const openAgentsEditor = async (): Promise<void> => {
    if (
      handle === null ||
      selectedProject === null ||
      handleProjectId !== selectedProject.id
    ) {
      return;
    }
    setEditorOpen(true);
    setEditorError(null);
    setEditorLoading(true);
    setDraft("");
    try {
      if (!(await ensureProjectPermission(handle))) {
        setEditorError("Folder access was not granted.");
        return;
      }
      setPermission("granted");
      const text = await readAgentsMd(handle);
      setDraft(text ?? "");
      setAgentsPresent(text !== null);
    } catch (cause) {
      setEditorError(errorMessage(cause));
    } finally {
      setEditorLoading(false);
    }
  };

  const saveAgentsMd = async (): Promise<void> => {
    if (
      handle === null ||
      selectedProject === null ||
      handleProjectId !== selectedProject.id
    ) {
      return;
    }
    setEditorSaving(true);
    setEditorError(null);
    try {
      const bytes = new TextEncoder().encode(draft);
      await writeAgentsMd(handle, draft);
      await uploadWebProjectFile(selectedProject.id, "AGENTS.md", bytes);
      setAgentsPresent(true);
      // Keep the in-memory snapshot in step with the folder so a later retry
      // uploads the just-saved file too.
      setSnapshot((current) => {
        if (current === null) {
          return current;
        }
        const previous = current.files.find(
          (entry) => entry.path === "AGENTS.md",
        );
        return {
          ...current,
          files: [
            ...current.files.filter((entry) => entry.path !== "AGENTS.md"),
            {
              path: "AGENTS.md",
              file: new File([bytes], "AGENTS.md", {
                type: "text/markdown",
              }),
              size: bytes.byteLength,
            },
          ],
          totalBytes:
            current.totalBytes - (previous?.size ?? 0) + bytes.byteLength,
        };
      });
      onProjectUpdated({ ...selectedProject, agents_md_present: true });
      setEditorOpen(false);
      setUploadNotice("Saved AGENTS.md to the folder and re-uploaded it.");
    } catch (cause) {
      setEditorError(errorMessage(cause));
    } finally {
      setEditorSaving(false);
    }
  };

  const startSync = async (): Promise<void> => {
    if (handle === null) {
      setSyncError("Choose the folder before syncing results.");
      return;
    }
    const taskId = syncTaskId.trim();
    if (taskId === "") {
      setSyncError("Enter the task id whose results should be synced.");
      return;
    }
    setSyncing(true);
    setSyncError(null);
    setSyncSummary(null);
    setSyncProgress(null);
    try {
      if (!(await ensureProjectPermission(handle))) {
        setPermission("denied");
        setSyncError("Folder access was not granted.");
        return;
      }
      setPermission("granted");
      const files = await getTaskOutputs(taskId);
      const result = await syncTaskOutputs(handle, taskId, files, {
        onProgress: setSyncProgress,
      });
      setSyncSummary(result);
    } catch (cause) {
      setSyncError(errorMessage(cause));
    } finally {
      setSyncing(false);
      setSyncProgress(null);
    }
  };

  const permissionLabel =
    permission === "granted"
      ? "write access"
      : permission === "denied"
        ? "access denied"
        : "permission needed";
  const limitsError =
    snapshot === null ? null : snapshotLimitError(snapshot);
  const droppedCount = snapshot?.droppedFiles.length ?? 0;

  return (
    <div className="mt-3 rounded-lg border border-border bg-surface-subtle px-3.5 py-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="type-label font-semibold text-foreground">Web folder</p>
        <Badge variant="outline">Chrome / Edge</Badge>
      </div>
      <p className="mt-1 type-meta text-muted-foreground">
        Upload a browser folder snapshot. {WEB_PROJECT_LIMITS_TEXT}. Skips{" "}
        {WEB_PROJECT_SKIP_DIRECTORIES.map((name) => (
          <code key={name} className="type-mono">
            {name}{" "}
          </code>
        ))}
        automatically.
      </p>

      {!supported ? (
        <p className="mt-2 rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
          {FILE_SYSTEM_ACCESS_FALLBACK_TEXT}
        </p>
      ) : (
        <>
          <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
            {handleLoading ? (
              <span>Checking the remembered folder…</span>
            ) : handle === null ? (
              <span>No folder selected yet.</span>
            ) : (
              <>
                <FolderOpen className="h-3.5 w-3.5 shrink-0" />
                <span className="font-medium text-foreground">
                  {handle.name}
                </span>
                <span aria-hidden="true">·</span>
                <span>{permissionLabel}</span>
                {agentsPresent !== null ? (
                  <Badge
                    variant={agentsPresent ? "success" : "muted"}
                  >
                    {webProjectAgentsMdLabel(agentsPresent)}
                  </Badge>
                ) : null}
              </>
            )}
            {scanning ? (
              <span className="text-muted-foreground">Scanning…</span>
            ) : null}
          </div>

          <div className="mt-2 flex flex-wrap items-center gap-2">
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => void chooseFolder()}
              disabled={uploading}
            >
              <FolderOpen className="h-3.5 w-3.5" />
              Choose folder
            </Button>
            {handle !== null && permission !== "granted" ? (
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => void grantAccess()}
                disabled={uploading}
              >
                <ShieldCheck className="h-3.5 w-3.5" />
                Grant access
              </Button>
            ) : null}
            <Button
              type="button"
              size="sm"
              onClick={() => void uploadSnapshot()}
              disabled={handle === null || uploading || scanning}
            >
              <Upload className="h-3.5 w-3.5" />
              {uploading
                ? "Uploading…"
                : createdProject === null
                  ? "Upload snapshot"
                  : "Retry upload"}
            </Button>
            {uploading ? (
              <Button
                type="button"
                size="sm"
                variant="ghost"
                onClick={() => abortRef.current?.abort()}
              >
                <Square className="h-3.5 w-3.5" />
                Cancel
              </Button>
            ) : null}
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => void startSync()}
              disabled={handle === null || syncing}
              title="Fetch a task's outputs and write them into this folder"
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${syncing ? "animate-spin" : ""}`}
              />
              {syncing ? "Syncing…" : "Sync results"}
            </Button>
          </div>

          {handle !== null ? (
            <div className="mt-2 space-y-1">
              <label
                className="type-label font-semibold text-foreground"
                htmlFor="web-sync-task-id"
              >
                Sync results for task
              </label>
              <div className="flex flex-wrap items-center gap-2">
                <Input
                  id="web-sync-task-id"
                  className="max-w-sm type-mono"
                  value={syncTaskId}
                  placeholder="Task id"
                  disabled={syncing}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                    }
                  }}
                  onChange={(event) => setSyncTaskId(event.target.value)}
                />
                <span className="type-meta text-muted-foreground">
                  Writes under .dhi/&lt;task-id&gt;/. Keep this tab open.
                </span>
              </div>
            </div>
          ) : null}

          {handle !== null ? (
            <div className="mt-2 space-y-1">
              <label
                className="type-label font-semibold text-foreground"
                htmlFor="web-project-name"
              >
                Project name
              </label>
              <Input
                id="web-project-name"
                className="max-w-sm"
                value={projectName}
                maxLength={120}
                disabled={uploading}
                onKeyDown={(event) => {
                  // This input lives inside the create-task form; Enter must
                  // not submit the task while naming the snapshot.
                  if (event.key === "Enter") {
                    event.preventDefault();
                  }
                }}
                onChange={(event) => setProjectName(event.target.value)}
              />
            </div>
          ) : null}

          {snapshot !== null ? (
            <p className="mt-2 type-meta text-muted-foreground">
              Snapshot: {snapshot.files.length} file
              {snapshot.files.length === 1 ? "" : "s"} ·{" "}
              {formatBytes(snapshot.totalBytes)}
              {droppedCount > 0
                ? ` · ${droppedCount} file${droppedCount === 1 ? "" : "s"} over the per-file limit were skipped`
                : ""}
              {snapshot.skippedDirectories.length > 0
                ? ` · ${snapshot.skippedDirectories.length} excluded folder${snapshot.skippedDirectories.length === 1 ? "" : "s"}`
                : ""}
            </p>
          ) : null}

          {limitsError !== null ? (
            <p className="mt-2 rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 type-meta text-warning">
              {limitsError}
            </p>
          ) : null}

          {uploading && progress !== null ? (
            <div className="mt-2 space-y-1">
              <p className="type-meta text-muted-foreground">
                Uploading {progress.uploadedFiles}/{progress.totalFiles} ·{" "}
                {formatBytes(progress.uploadedBytes)} /{" "}
                {formatBytes(progress.totalBytes)}
              </p>
              <div className="h-1.5 overflow-hidden rounded-full bg-border">
                <div
                  className="h-full bg-primary transition-[width] duration-150"
                  style={{
                    width: `${
                      progress.totalBytes === 0
                        ? 100
                        : Math.min(
                            100,
                            Math.round(
                              (progress.uploadedBytes /
                                progress.totalBytes) *
                                100,
                            ),
                          )
                    }%`,
                  }}
                />
              </div>
              {progress.currentPath !== null ? (
                <p className="truncate type-mono text-muted-foreground">
                  {progress.currentPath}
                </p>
              ) : null}
            </div>
          ) : null}

          {uploadNotice !== null ? (
            <p className="mt-2 type-meta text-success">{uploadNotice}</p>
          ) : null}

          {uploadError !== null ? (
            <p
              className="mt-2 rounded-md border border-destructive/30 bg-destructive/8 px-2.5 py-1.5 type-meta text-destructive"
              role="alert"
            >
              {uploadError}
            </p>
          ) : null}

          {folderError !== null ? (
            <p
              className="mt-2 rounded-md border border-destructive/30 bg-destructive/8 px-2.5 py-1.5 type-meta text-destructive"
              role="alert"
            >
              {folderError}
            </p>
          ) : null}

          {syncing && syncProgress !== null ? (
            <p className="mt-2 type-meta text-muted-foreground">
              Syncing {syncProgress.processed}/{syncProgress.total}
              {syncProgress.currentPath === null
                ? ""
                : ` · ${syncProgress.currentPath}`}
            </p>
          ) : null}

          {syncSummary !== null ? (
            <div className="mt-2 rounded-md border border-border bg-card px-2.5 py-1.5 type-meta">
              <span className="flex flex-wrap items-center gap-2">
                <Badge
                  variant={syncSummary.errors > 0 ? "warning" : "success"}
                >
                  {syncSummary.written} synced
                </Badge>
                <Badge variant="muted">{syncSummary.skipped} skipped</Badge>
                <Badge
                  variant={
                    syncSummary.errors > 0 ? "destructive" : "muted"
                  }
                >
                  {syncSummary.errors} errors
                </Badge>
              </span>
              {syncSummary.skippedPaths.length > 0 ? (
                <p className="mt-1 break-all text-muted-foreground">
                  Skipped: {syncSummary.skippedPaths.join(", ")}
                </p>
              ) : null}
              {syncSummary.errorMessages.length > 0 ? (
                <ul className="mt-1 list-disc space-y-1 pl-4 text-destructive">
                  {syncSummary.errorMessages.map((message) => (
                    <li key={message} className="break-all">
                      {message}
                    </li>
                  ))}
                </ul>
              ) : null}
            </div>
          ) : null}

          {syncError !== null ? (
            <p
              className="mt-2 rounded-md border border-destructive/30 bg-destructive/8 px-2.5 py-1.5 type-meta text-destructive"
              role="alert"
            >
              {syncError}
            </p>
          ) : null}

          {selectedProject !== null ? (
            <div className="mt-3 border-t border-border pt-3">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
                <span className="font-medium text-foreground">
                  {selectedProject.name}
                </span>
                <Badge
                  variant={
                    selectedProject.agents_md_present ? "success" : "muted"
                  }
                >
                  {webProjectAgentsMdLabel(
                    selectedProject.agents_md_present,
                  )}
                </Badge>
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() => void openAgentsEditor()}
                  disabled={
                    handle === null ||
                    handleProjectId !== selectedProject.id ||
                    permission !== "granted" ||
                    editorLoading
                  }
                >
                  <Pencil className="h-3.5 w-3.5" />
                  {selectedProject.agents_md_present
                    ? "Edit AGENTS.md"
                    : "Add AGENTS.md"}
                </Button>
              </div>
              {handle === null ? (
                <p className="mt-1 type-meta text-muted-foreground">
                  Choose this project&apos;s local folder to edit its AGENTS.md.
                </p>
              ) : handleProjectId !== selectedProject.id ? (
                <p className="mt-1 type-meta text-muted-foreground">
                  This folder is not stored for this project. Select the
                  project whose folder was chosen, or upload a snapshot to
                  create its web project.
                </p>
              ) : permission !== "granted" ? (
                <p className="mt-1 type-meta text-muted-foreground">
                  Grant folder access to edit AGENTS.md.
                </p>
              ) : (
                <p className="mt-1 type-meta text-muted-foreground">
                  Editing writes to the local folder first, then re-uploads the
                  file to this web project.
                </p>
              )}
            </div>
          ) : (
            <p className="mt-3 type-meta text-muted-foreground">
              Upload a snapshot to create a web project; AGENTS.md editing
              appears here afterwards.
            </p>
          )}
        </>
      )}

      <Dialog open={editorOpen} onOpenChange={setEditorOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>AGENTS.md</DialogTitle>
            <DialogDescription>
              {selectedProject !== null
                ? `Planner guidance for “${selectedProject.name}”. Saving writes the local file and re-uploads it.`
                : "Planner guidance for the selected web project."}
            </DialogDescription>
          </DialogHeader>
          {editorLoading ? (
            <p className="type-meta text-muted-foreground">Reading…</p>
          ) : (
            <Textarea
              value={draft}
              rows={16}
              placeholder="Project guidance for the planner…"
              disabled={editorSaving}
              onChange={(event) => setDraft(event.target.value)}
            />
          )}
          {editorError !== null ? (
            <p className="type-meta text-destructive" role="alert">
              {editorError}
            </p>
          ) : null}
          <DialogFooter>
            <DialogClose asChild>
              <Button type="button" variant="outline" disabled={editorSaving}>
                Cancel
              </Button>
            </DialogClose>
            <Button
              type="button"
              onClick={() => void saveAgentsMd()}
              disabled={
                editorSaving ||
                editorLoading ||
                selectedProject === null ||
                handle === null ||
                handleProjectId !== selectedProject.id
              }
            >
              {editorSaving ? "Saving…" : "Save AGENTS.md"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
