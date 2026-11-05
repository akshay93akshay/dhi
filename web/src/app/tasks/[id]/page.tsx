import { TaskDetailClient } from "./DetailClient";

/**
 * Task-detail route. `generateStaticParams` returns the `preview` placeholder
 * so `output: export` has a route to emit: the page itself is a client shell
 * that reads the id via `useParams` and fetches at runtime. Under `next dev` /
 * `next start` this stays fully dynamic.
 */
export function generateStaticParams(): Array<{ id: string }> {
  // One placeholder shell: static export must emit at least one route. The
  // prerendered HTML is the loading skeleton only (effects/fetches are
  // client-side); real ids resolve via `useParams` at runtime.
  return [{ id: "preview" }];
}

export default function TaskDetailPage() {
  return <TaskDetailClient />;
}
