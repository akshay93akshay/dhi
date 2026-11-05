import { ApprovalsClient } from "./ApprovalsClient";

/**
 * Approvals queue route. A thin server shim rendering the client shell,
 * matching the client-shell route pattern: all data fetching happens
 * client-side, so there is nothing to prerender statically (no dynamic
 * params, hence no `generateStaticParams` — the route exports as-is).
 */
export default function ApprovalsPage() {
  return <ApprovalsClient />;
}
