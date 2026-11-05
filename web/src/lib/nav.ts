/**
 * Pure helpers for breadcrumb trails (`web/src/app/_components/Crumbs.tsx`).
 * No React and no router, so nothing here can hydrate differently between
 * server and client render.
 */

import { shortenId } from "./ui.ts";

/** One segment of a breadcrumb trail below the top nav. */
export type CrumbSegment = {
  label: string;
  /** Absent = the segment is plain text (typically the current leaf). */
  href?: string;
};

/**
 * Builds the "Tasks / #<id> / <leaf>" trail shown on inner task pages.
 * The Tasks segment links to the task ledger, the id segment links to the
 * task detail page, and the leaf is plain text. Degradates: an empty
 * `taskId` drops the id segment, an empty `leaf` keeps the trail as a
 * plain "Tasks / #<id>" path — never renders an empty crumb.
 */
export function crumbTrailSegments(taskId: string, leaf: string): CrumbSegment[] {
  const segments: CrumbSegment[] = [{ label: "Tasks", href: "/tasks" }];
  const trimmedId = taskId.trim();
  if (trimmedId !== "") {
    segments.push({
      label: `#${shortenId(trimmedId)}`,
      href: `/tasks/${trimmedId}`,
    });
  }
  const trimmedLeaf = leaf.trim();
  if (trimmedLeaf !== "") {
    segments.push({ label: trimmedLeaf });
  }
  return segments;
}
