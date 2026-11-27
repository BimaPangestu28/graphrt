// Runs the tests of the web packages: bundles each `packages/*/test/*.test.ts`
// with esbuild (Node's own TypeScript support depends on how Node was built),
// then hands the results to `node --test`.
//
// Usage: node scripts/test.mjs [filter]
//   filter  only test files whose path contains this text
import { spawnSync } from "node:child_process";
import { readdirSync, rmSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, ".test-build");
const filter = process.argv[2] ?? "";

const entries = [];
for (const name of readdirSync(join(root, "packages"))) {
  const testDir = join(root, "packages", name, "test");
  let files = [];
  try {
    files = readdirSync(testDir);
  } catch {
    continue;
  }
  for (const file of files.filter((file) => file.endsWith(".test.ts"))) {
    const path = join(testDir, file);
    if (relative(root, path).includes(filter)) {
      entries.push(path);
    }
  }
}
if (entries.length === 0) {
  console.error(`no test files match \`${filter}\``);
  process.exit(2);
}

rmSync(outDir, { recursive: true, force: true });
await build({
  entryPoints: entries,
  outdir: outDir,
  outbase: join(root, "packages"),
  outExtension: { ".js": ".mjs" },
  bundle: true,
  platform: "node",
  format: "esm",
  target: "node20",
  sourcemap: "inline",
  // Resolved by Node at run time: it reads its own files next to its code.
  external: ["happy-dom"],
  logLevel: "warning",
});

const built = entries.map((path) => join(outDir, relative(join(root, "packages"), path)).replace(/\.ts$/, ".mjs"));
const run = spawnSync(process.execPath, ["--enable-source-maps", "--test", ...built], {
  cwd: root,
  env: { ...process.env, GRAPHRT_REPO_ROOT: resolve(root, "..") },
  stdio: "inherit",
});
process.exit(run.status ?? 1);
