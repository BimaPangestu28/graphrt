// Builds the `@graphrt-dev/graphrt` npm package (packages/graphrt), checks it as a user
// would get it, and publishes it.
//
// The package in the workspace is private and has no version: what is published
// is a copy staged in .pack/node_modules/@graphrt-dev/graphrt, with the version of the
// graphrt release (`[workspace.package] version` of the root Cargo.toml), the
// LICENSE of the repository and exports without the `source` condition.
//
// Usage: node scripts/publish.mjs [--build | --pack | --execute] [--version X.Y.Z]
//   (none)     build, stage and check: the files npm would upload, every entry
//              point loads in Node.js, a bundler leaves out what is not imported
//   --build    build dist/ only
//   --pack     build, stage, check, and write .pack/graphrt-dev-graphrt-X.Y.Z.tgz
//   --execute  build, stage, check, and publish unless the version is on npm already
//   --version  the version to publish. It must be the version in Cargo.toml, so
//              a release tag and its package always agree. Default: that version.
//
// In the release workflow publishing takes no token: release.yml is the trusted
// publisher of `@graphrt-dev/graphrt` on npmjs.com (OIDC), and npm gives every version a
// provenance statement. See web/README.md.
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(root, "..");
const packageDir = join(root, "packages", "graphrt");
const packRoot = join(root, ".pack");
// The name users install. npm refused the unscoped `graphrt` as too close to
// `graphql`, so the package lives in the scope of the npm organization graphrt-dev.
const PACKAGE_NAME = "@graphrt-dev/graphrt";
const stageDir = join(packRoot, "node_modules", ...PACKAGE_NAME.split("/"));
const PARTS = ["client", "ui", "chat", "monitor", "console"];
// Scripts of the embedded pages: they run on import, so they must never ship.
const PAGE_SCRIPTS = ["chat/page", "chat/embed", "monitor/page", "monitor/sw", "console/page"];

function fail(message) {
  console.error(`publish: ${message}`);
  process.exit(1);
}

function parseArguments(argv) {
  const options = { mode: "--check", version: undefined };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (["--build", "--pack", "--execute"].includes(argument) && options.mode === "--check") {
      options.mode = argument;
    } else if (argument === "--version" && options.version === undefined && index + 1 < argv.length) {
      options.version = argv[index + 1];
      index += 1;
    } else {
      console.error("usage: node scripts/publish.mjs [--build | --pack | --execute] [--version X.Y.Z]");
      process.exit(2);
    }
  }
  return options;
}

function run(command, args, cwd, capture = false) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.error !== undefined) {
    fail(`cannot run ${command}: ${result.error.message}`);
  }
  return { ok: result.status === 0, stdout: result.stdout ?? "", stderr: result.stderr ?? "" };
}

/** `[workspace.package] version` of the root Cargo.toml: the version of a graphrt release. */
function releaseVersion() {
  const cargo = readFileSync(join(repoRoot, "Cargo.toml"), "utf8");
  const section = cargo.split(/^\[workspace\.package\]\s*$/m)[1]?.split(/^\[/m)[0];
  const version = section?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (version === undefined || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    fail("cannot read [workspace.package] version of Cargo.toml");
  }
  return version;
}

function buildDist() {
  rmSync(join(packageDir, "dist"), { recursive: true, force: true });
  const tsc = join(root, "node_modules", "typescript", "bin", "tsc");
  if (!run(process.execPath, [tsc, "-p", join(packageDir, "tsconfig.build.json")], root).ok) {
    fail("graphrt does not build");
  }
  for (const part of PARTS) {
    for (const file of ["index.js", "index.d.ts"]) {
      if (!existsSync(join(packageDir, "dist", part, file))) {
        fail(`dist/${part}/${file} was not built`);
      }
    }
  }
}

/** The manifest as published: public, versioned, and resolving to `dist` only. */
function publishedManifest(version) {
  const manifest = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8"));
  if (manifest.name !== PACKAGE_NAME) {
    fail(`packages/graphrt/package.json is named ${manifest.name}, not ${PACKAGE_NAME}`);
  }
  delete manifest.private;
  manifest.version = version;
  manifest.exports = Object.fromEntries(
    Object.entries(manifest.exports).map(([path, target]) => {
      if (typeof target === "string") {
        return [path, target];
      }
      const { source: _source, ...published } = target;
      return [path, published];
    }),
  );
  return manifest;
}

function stage(version) {
  rmSync(packRoot, { recursive: true, force: true });
  mkdirSync(stageDir, { recursive: true });
  cpSync(join(packageDir, "dist"), join(stageDir, "dist"), { recursive: true });
  cpSync(join(packageDir, "README.md"), join(stageDir, "README.md"));
  cpSync(join(repoRoot, "LICENSE"), join(stageDir, "LICENSE"));
  writeFileSync(join(stageDir, "package.json"), `${JSON.stringify(publishedManifest(version), null, 2)}\n`);
}

function checkFiles() {
  const packed = run("npm", ["pack", "--dry-run", "--json"], stageDir, true);
  if (!packed.ok) {
    fail(`npm pack failed\n${packed.stderr}`);
  }
  const files = JSON.parse(packed.stdout)[0].files.map((file) => file.path);
  const stray = files.filter(
    (file) => !/^(dist\/.+\.(js|d\.ts)|README\.md|LICENSE|package\.json)$/.test(file),
  );
  const pages = files.filter((file) => PAGE_SCRIPTS.some((script) => file.startsWith(`dist/${script}.`)));
  const missing = PARTS.flatMap((part) => [`dist/${part}/index.js`, `dist/${part}/index.d.ts`]).filter(
    (file) => !files.includes(file),
  );
  if (stray.length > 0 || pages.length > 0 || missing.length > 0 || !files.includes("LICENSE")) {
    fail(
      `the package would publish the wrong files (stray: ${[...stray, ...pages].join(", ") || "none"}, missing: ${missing.join(", ") || "none"})`,
    );
  }
  return files.length;
}

/** Every entry point loads in Node.js the way a user imports it, by package name. */
function checkNodeImports() {
  for (const part of PARTS) {
    const specifier = `${PACKAGE_NAME}/${part}`;
    const script = `const m = await import(${JSON.stringify(specifier)}); if (Object.keys(m).length === 0) process.exit(3);`;
    const loaded = run(process.execPath, ["--input-type=module", "-e", script], packRoot, true);
    if (!loaded.ok) {
      fail(`${specifier} does not load in Node.js\n${loaded.stderr}`);
    }
  }
}

async function bundleFrom(contents) {
  const result = await build({
    stdin: { contents, resolveDir: packRoot, loader: "js" },
    bundle: true,
    write: false,
    format: "esm",
    platform: "browser",
    minify: true,
    logLevel: "silent",
  });
  const [file] = result.outputFiles;
  if (file === undefined) {
    fail("esbuild produced nothing");
  }
  return file.text;
}

/**
 * A bundler keeps only what is imported: importing every part and using none
 * leaves nothing (no side effects), and the client alone carries no web
 * component, no styles and no Mermaid loader.
 */
async function checkTreeShaking() {
  const unused = await bundleFrom(PARTS.map((part) => `import "${PACKAGE_NAME}/${part}";`).join("\n"));
  if (unused.trim().length > 0) {
    fail(`importing the parts without using them still bundles code:\n${unused.slice(0, 400)}`);
  }
  const client = await bundleFrom(
    `import { GraphrtClient } from "${PACKAGE_NAME}/client";\nglobalThis.client = GraphrtClient;`,
  );
  for (const marker of ["customElements", "mermaid", "graphrt-block", "--grt-"]) {
    if (client.includes(marker)) {
      fail(`${PACKAGE_NAME}/client bundles UI code (found \`${marker}\`)`);
    }
  }
  console.log(`${PACKAGE_NAME}/client alone bundles to ${(client.length / 1024).toFixed(1)} KiB minified`);
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  const version = releaseVersion();
  if (options.version !== undefined && options.version !== version) {
    fail(`--version ${options.version} is not the version of Cargo.toml (${version})`);
  }
  buildDist();
  if (options.mode === "--build") {
    return;
  }
  stage(version);
  const count = checkFiles();
  checkNodeImports();
  await checkTreeShaking();
  console.log(`${PACKAGE_NAME}@${version}: ${count} files, staged in ${stageDir}`);
  if (options.mode === "--pack") {
    const packed = run("npm", ["pack", "--json", "--pack-destination", packRoot], stageDir, true);
    if (!packed.ok) {
      fail(`npm pack failed\n${packed.stderr}`);
    }
    console.log(join(packRoot, JSON.parse(packed.stdout)[0].filename));
    return;
  }
  if (options.mode !== "--execute") {
    return;
  }
  const spec = `${PACKAGE_NAME}@${version}`;
  // A version on npm is final: running a release again leaves it alone.
  const published = run("npm", ["view", spec, "version", "--json"], stageDir, true);
  if (published.ok && published.stdout.includes(version)) {
    console.log(`${spec} is on npm already`);
    return;
  }
  if (!published.ok && !/E404|404 Not Found/.test(published.stderr + published.stdout)) {
    fail(`cannot ask npm about ${spec}\n${published.stderr}`);
  }
  const publish = ["publish", "--access", "public"];
  if (version.includes("-")) {
    // A prerelease never becomes what `npm install @graphrt-dev/graphrt` gets.
    publish.push("--tag", "next");
  }
  if (process.env.GITHUB_ACTIONS === "true") {
    publish.push("--provenance");
  }
  if (!run("npm", publish, stageDir).ok) {
    fail(`${spec} was not published`);
  }
  console.log(`published ${spec}`);
}

await main();
