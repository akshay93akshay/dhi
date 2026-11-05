import { ToolsClient } from "./ToolsClient";

/**
 * Tools admin route. Like the other ops pages this is a thin server shim
 * around a client shell: the catalog is fetched in the browser, so there is
 * nothing to prerender and the static export stays happy.
 */
export default function ToolsPage() {
  return <ToolsClient />;
}
