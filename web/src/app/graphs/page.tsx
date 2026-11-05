"use client";

import { useEffect } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";

/**
 * Legacy `/graphs` route. Task graphs now live on each task, so this page
 * redirects the old bookmark to the task ledger and leaves a short fallback
 * for the moment before the client-side navigation runs.
 */
export default function GraphsPage() {
  const router = useRouter();

  useEffect(() => {
    router.replace("/tasks");
  }, [router]);

  return (
    <main className="space-y-3">
      <h1 className="text-[24px] font-semibold tracking-[-0.02em]">
        Graphs moved to tasks
      </h1>
      <p className="text-sm leading-6 text-muted-foreground">
        Open a task to see its work graph. Redirecting to{" "}
        <Link
          href="/tasks"
          className="font-medium underline underline-offset-2"
        >
          Tasks
        </Link>
        …
      </p>
    </main>
  );
}
