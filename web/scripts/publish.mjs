// Builds the packages of this workspace as they are published to npm
// (`dist/` with JavaScript and type declarations) and publishes them.
//
// Usage: node scripts/publish.mjs [--build | --execute]
//   (none)     build every package and check what `npm publish` would upload
//   --build    build only
//   --execute  build, check, and publish every version that is not on npm yet
//
// Publishing takes no token: the release workflow of the repository is the
// trusted publisher of the packages on npmjs.com (OIDC), which also gives each
// published version a provenance statement. See docs/web.md.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
// In dependency order: each is built against the `dist` of the ones before it.
const PACKAGES = ["client", "ui", "chat", "monitor"];
const mode = process.argv[2] ?? "--check";
if (!["--check", "--build", "--execute"].includes(mode) || process.argv.length > 3) {
  console.error("usage: node scripts/publish.mjs [--build | --execute]");
  process.exit(2);
}

function run(command, args, cwd, capture = false) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  return { ok: result.status === 0, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}

function fail(message) {
  console.error(`publish: ${message}`);
  process.exit(1);
}

const manifests = new Map();
for (const name of PACKAGES) {
  const dir = join(root, "packages", name);
  manifests.set(name, { dir, manifest: JSON.parse(readFileSync(join(dir, "package.json"), "utf8")) });
}

// A package must depend on the exact version of its siblings that is published with it.
for (const { manifest } of manifests.values()) {
  for (const [dependency, range] of Object.entries(manifest.dependencies ?? {})) {
    const sibling = [...manifests.values()].find((other) => other.manifest.name === dependency);
    if (sibling !== undefined && sibling.manifest.version !== range) {
      fail(`${manifest.name} depends on ${dependency}@${range}, which is at ${sibling.manifest.version}`);
    }
  }
}

for (const name of PACKAGES) {
  const { dir, manifest } = manifests.get(name);
  rmSync(join(dir, "dist"), { recursive: true, force: true });
  const tsc = join(root, "node_modules", "typescript", "bin", "tsc");
  if (!run(process.execPath, [tsc, "-p", join(dir, "tsconfig.build.json")], root).ok) {
    fail(`${manifest.name} does not build`);
  }
  for (const file of ["index.js", "index.d.ts"]) {
    if (!existsSync(join(dir, "dist", file))) {
      fail(`${manifest.name}: dist/${file} was not built`);
    }
  }
}
if (mode === "--build") {
  process.exit(0);
}

for (const name of PACKAGES) {
  const { dir, manifest } = manifests.get(name);
  // What would be uploaded: the built files and nothing of the sources or tests.
  const packed = run("npm", ["pack", "--dry-run", "--json"], dir, true);
  if (!packed.ok) {
    fail(`${manifest.name}: npm pack failed\n${packed.stderr}`);
  }
  const files = JSON.parse(packed.stdout)[0].files.map((file) => file.path);
  const stray = files.filter((file) => !/^(dist\/|README\.md$|package\.json$|LICENSE$)/.test(file));
  if (stray.length > 0 || !files.includes("dist/index.js") || !files.includes("README.md")) {
    fail(`${manifest.name} would publish the wrong files: ${files.join(", ")}`);
  }
  // The built entry loads on its own in Node.js, with its siblings from their `dist`.
  const loaded = run(process.execPath, ["-e", `await import(${JSON.stringify(manifest.name)})`], dir, true);
  if (!loaded.ok) {
    fail(`${manifest.name}: the built package does not load\n${loaded.stderr}`);
  }
  console.log(`${manifest.name}@${manifest.version}: ${files.length} files`);
}
if (mode !== "--execute") {
  process.exit(0);
}

for (const name of PACKAGES) {
  const { dir, manifest } = manifests.get(name);
  const spec = `${manifest.name}@${manifest.version}`;
  // A version on npm is final: a release that did not change a package leaves it alone.
  const published = run("npm", ["view", spec, "version", "--json"], dir, true);
  if (published.ok && published.stdout.includes(manifest.version)) {
    console.log(`${spec} is on npm already`);
    continue;
  }
  if (!published.ok && !/E404|404 Not Found/.test(published.stderr + published.stdout)) {
    fail(`cannot ask npm about ${spec}\n${published.stderr}`);
  }
  if (!run("npm", ["publish", "--access", "public"], dir).ok) {
    fail(`${spec} was not published`);
  }
  console.log(`published ${spec}`);
}
