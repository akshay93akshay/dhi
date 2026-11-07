"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { Plus } from "lucide-react";

import { createTask, getHealth, listProfiles } from "@/lib/api";
import type { Health, LlmProfile } from "@/lib/types";
import {
  buildCreateTaskInput,
  describeApiError,
  serviceState,
} from "@/lib/ui";
import {
  DhiBrandStatus,
  type BrandActivity,
} from "@/components/dhi-brand-status";
import { PendingTasks } from "@/components/pending-tasks";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
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
          className="mt-5 text-[32px] font-medium tracking-[-0.03em] text-foreground sm:text-[40px]"
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

  const handleSubmit = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    reportActivity("submit");
    setFormError(null);
    const parsed = buildCreateTaskInput({ goal, plannerPin, workerPin });
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
            <CardTitle className="text-base">Launch a task</CardTitle>
            <CardDescription className="mt-0.5">
              One goal, one exploration, one origin fact.
            </CardDescription>
          </div>
        </div>
      </CardHeader>
      <CardContent className="flex-1 pt-4">
        <form onSubmit={(event) => void handleSubmit(event)} className="space-y-4">
          <div className="space-y-1.5">
            <label
              className="text-xs font-semibold text-foreground"
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

          {profiles === null && profilesError === null ? (
            <p className="text-[11.5px] text-muted-foreground">
              Loading profiles…
            </p>
          ) : null}

          {profilesError !== null ? (
            <p className="rounded-md border border-warning/30 bg-warning/8 px-2.5 py-1.5 text-[11.5px] leading-4 text-warning">
              Could not load profiles ({profilesError}). Pins unavailable.
            </p>
          ) : null}

          {profiles !== null && profiles.length === 0 ? (
            <p className="rounded-md border border-border bg-surface-subtle px-2.5 py-1.5 text-[11.5px] leading-4 text-muted-foreground">
              No profiles seeded yet — the task uses the role pool defaults.
              Seed profiles via Settings to enable pins.
            </p>
          ) : null}

          {profiles !== null && profiles.length > 0 ? (
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-1.5">
                <label
                  className="text-xs font-semibold text-foreground"
                  htmlFor="create-planner-pin"
                >
                  Planner profile
                </label>
                <select
                  id="create-planner-pin"
                  className="h-9 rounded-md border-input bg-card text-[13px]"
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
                  className="text-xs font-semibold text-foreground"
                  htmlFor="create-worker-pin"
                >
                  Worker profile
                </label>
                <select
                  id="create-worker-pin"
                  className="h-9 rounded-md border-input bg-card text-[13px]"
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

          {formError !== null ? (
            <div
              className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 text-xs leading-5 text-destructive"
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
          <p className="text-center text-[11px] leading-4 text-muted-foreground">
            Tasks run immediately. Every tool call still passes the guard.
          </p>
        </form>
      </CardContent>
    </Card>
  );
}
