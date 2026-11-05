import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** @type {import('next').NextConfig} */
const isExport = process.env.NEXT_EXPORT === "1";
const webRoot = path.dirname(fileURLToPath(import.meta.url));

const nextConfig = {
  output: isExport ? "export" : undefined,
  trailingSlash: isExport,
  images: { unoptimized: true },
};

// Worktrees share node_modules through a symlink. Turbopack will not read
// packages outside its workspace root, so widen the root to the common
// parent directory only in that case. A normal install keeps Next's default.
function contains(parent, candidate) {
  const relative = path.relative(parent, candidate);
  return (
    relative === "" ||
    (!relative.startsWith(`..${path.sep}`) &&
      relative !== ".." &&
      !path.isAbsolute(relative))
  );
}

function commonAncestor(left, right) {
  let current = path.resolve(left);
  while (!contains(current, right)) {
    const parent = path.dirname(current);
    if (parent === current) {
      return current;
    }
    current = parent;
  }
  return current;
}

try {
  const nodeModules = path.join(webRoot, "node_modules");
  if (fs.lstatSync(nodeModules).isSymbolicLink()) {
    const target = fs.realpathSync(nodeModules);
    const root = commonAncestor(webRoot, target);
    if (root !== path.parse(root).root) {
      nextConfig.turbopack = { root };
    }
  }
} catch {
  // No node_modules; leave the default and let Next report it.
}

if (!isExport) {
  nextConfig.rewrites = async () => [
    {
      source: "/api/:path*",
      destination: "http://127.0.0.1:8080/api/:path*",
    },
  ];
}

export default nextConfig;
