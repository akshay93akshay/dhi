import { TaskWorkClient } from "./WorkClient";

/**
 * Work-view route. `generateStaticParams` returns the `preview` placeholder
 * so `output: export` has a route to emit: the page is a client shell reading
 * the id via `useParams` and fetching at runtime. Under `next dev` /
 * `next start` this stays fully dynamic.
 */
export function generateStaticParams(): Array<{ id: string }> {
  // One placeholder shell: static export must emit at least one route. The
  // prerendered HTML is the loading skeleton only (effects/fetches are
  // client-side); real ids resolve via `useParams` at runtime.
  return [{ id: "preview" }];
}

export default function TaskWorkPage() {
  return <TaskWorkClient />;
}
