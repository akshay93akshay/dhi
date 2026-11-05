import { TaskGraphClient } from "./GraphClient";

/**
 * Task-scoped graph route. `generateStaticParams` returns the `preview`
 * placeholder so `output: export` has a route to emit: the page is a client
 * shell reading the id via `useParams` and fetching at runtime. Under
 * `next dev` / `next start` this stays fully dynamic.
 */
export function generateStaticParams(): Array<{ id: string }> {
  // The prerendered HTML is the loading skeleton only (effects/fetches are
  // client-side); real ids resolve via `useParams` at runtime.
  return [{ id: "preview" }];
}

export default function TaskGraphPage() {
  return <TaskGraphClient />;
}
