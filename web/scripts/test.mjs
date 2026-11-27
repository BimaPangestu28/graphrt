// Runs the tests of the `@graphrt-dev/graphrt` package: bundles each
// `packages/graphrt/test/*/*.test.ts`
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

const testRoot = join(root, "packages", "graphrt", "test");
const entries = [];
for (const part of readdirSync(testRoot)) {
  const testDir = join(testRoot, part);
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
  outbase: testRoot,
  outExtension: { ".js": ".mjs" },
  bundle: true,
  // Bundled from the sources, not from `dist`.
  conditions: ["source"],
  platform: "node",
  format: "esm",
  target: "node20",
  sourcemap: "inline",
  // Resolved by Node at run time: happy-dom reads its own files next to its
  // code, and qrcode (the reference of the QR tests) is CommonJS.
  external: ["happy-dom", "qrcode"],
  logLevel: "warning",
});

const built = entries.map((path) => join(outDir, relative(testRoot, path)).replace(/\.ts$/, ".mjs"));
const run = spawnSync(process.execPath, ["--enable-source-maps", "--test", ...built], {
  cwd: root,
  env: { ...process.env, GRAPHRT_REPO_ROOT: resolve(root, "..") },
  stdio: "inherit",
});
process.exit(run.status ?? 1);
