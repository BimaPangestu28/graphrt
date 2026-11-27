// Builds what `graphrt serve` serves itself, bundled from the sources of the
// `graphrt` package
// (packages/graphrt/src) into the assets of the crates, where the Rust build
// embeds them:
//
//   graphrt-cli/assets/chat          the chat page and the widget script (`--chat`)
//   graphrt-cli/assets/monitor       the monitoring page, its service worker, manifest and icons (`--monitor`)
//   graphrt-console/assets/console   the admin console page (`--console`)
//
// The results are committed, so building graphrt needs no Node.js.
//
// Usage: node scripts/build.mjs [--check]
//   --check  build in memory and fail when the committed files differ
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { encodePng, iconPixels, pngHolds } from "./icons.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const assets = resolve(root, "..", "crates", "graphrt-cli", "assets");
const consoleAssets = resolve(root, "..", "crates", "graphrt-console", "assets");
const check = process.argv.includes("--check");

const banner = (what) =>
  `/* graphrt ${what}, built from web/ of https://github.com/Graphrt/graphrt (Apache-2.0). Includes zod (MIT, Colin McDonnell). */`;

const common = {
  bundle: true,
  // Bundled from the sources, not from `dist`.
  conditions: ["source"],
  minify: true,
  write: false,
  target: ["es2022", "chrome109", "firefox115", "safari16"],
  legalComments: "none",
  logLevel: "warning",
};

const chatPage = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark">
<meta name="robots" content="noindex">
<title>Chat</title>
<link rel="icon" href="data:,">
<script type="module" src="chat.js"></script>
</head>
<body>
<noscript>This chat needs JavaScript.</noscript>
</body>
</html>
`;

const monitorPage = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#257de0">
<meta name="robots" content="noindex">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-title" content="Agent monitor">
<title>Agent monitor</title>
<link rel="manifest" href="manifest.webmanifest">
<link rel="icon" type="image/png" sizes="192x192" href="icon-192.png">
<link rel="apple-touch-icon" href="apple-touch-icon.png">
<script type="module" src="monitor.js"></script>
</head>
<body>
<noscript>This page needs JavaScript.</noscript>
</body>
</html>
`;

// Paths are relative to the manifest, so the page also installs below a path
// prefix of a reverse proxy.
const manifest = `${JSON.stringify(
  {
    name: "Agent monitor",
    short_name: "Monitor",
    description: "Runs and approvals of an agent served by graphrt.",
    id: "./",
    start_url: "./",
    scope: "./",
    display: "standalone",
    background_color: "#191919",
    theme_color: "#257de0",
    icons: [
      { src: "icon-192.png", sizes: "192x192", type: "image/png", purpose: "any" },
      { src: "icon-512.png", sizes: "512x512", type: "image/png", purpose: "any" },
      { src: "icon-maskable.png", sizes: "512x512", type: "image/png", purpose: "maskable" },
    ],
  },
  null,
  2,
)}\n`;

const consolePage = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#257de0">
<meta name="robots" content="noindex">
<title>Graphrt console</title>
<link rel="icon" href="data:,">
<script type="module" src="console.js"></script>
</head>
<body>
<noscript>The console needs JavaScript.</noscript>
</body>
</html>
`;

async function bundle(part, entry, format, what) {
  const result = await build({
    ...common,
    banner: { js: banner(what) },
    entryPoints: [join(root, "packages", "graphrt", "src", part, entry)],
    format,
  });
  const [file] = result.outputFiles;
  if (file === undefined) {
    throw new Error(`esbuild produced nothing for ${entry}`);
  }
  return file.text;
}

/** An icon: its pixels, written as a PNG and checked by its pixels. */
const icon = (size, maskable) => ({ pixels: iconPixels(size, maskable), size });

const outputs = {
  chat: {
    "index.html": chatPage,
    // A module: the page's policy allows scripts of its own origin only.
    "chat.js": await bundle("chat", "page.ts", "esm", "chat"),
    // A classic script: it reads `document.currentScript` to find its server and options.
    "embed.js": await bundle("chat", "embed.ts", "iife", "chat"),
  },
  monitor: {
    "index.html": monitorPage,
    "monitor.js": await bundle("monitor", "page.ts", "esm", "monitor"),
    // A classic script: service workers of every supported browser take one.
    "sw.js": await bundle("monitor", "sw.ts", "iife", "monitor"),
    "manifest.webmanifest": manifest,
    "icon-192.png": icon(192, false),
    "icon-512.png": icon(512, false),
    "icon-maskable.png": icon(512, true),
    // iOS puts the icon on its own background and rounds the corners itself.
    "apple-touch-icon.png": icon(180, true),
  },
  console: {
    "index.html": consolePage,
    "console.js": await bundle("console", "page.ts", "esm", "console"),
  },
};

/** The directory each folder of `outputs` is written to. */
const folderDir = (folder) => (folder === "console" ? join(consoleAssets, folder) : join(assets, folder));

let stale = false;
for (const [folder, files] of Object.entries(outputs)) {
  const outDir = folderDir(folder);
  mkdirSync(outDir, { recursive: true });
  for (const [name, content] of Object.entries(files)) {
    const path = join(outDir, name);
    const isIcon = typeof content !== "string";
    if (check) {
      let committed = Buffer.alloc(0);
      try {
        committed = readFileSync(path);
      } catch {
        // Missing counts as different.
      }
      const same = isIcon
        ? pngHolds(committed, content.pixels, content.size)
        : committed.toString("utf8") === content;
      if (!same) {
        console.error(`stale: ${path}`);
        stale = true;
      }
    } else {
      const bytes = isIcon ? encodePng(content.pixels, content.size) : Buffer.from(content);
      writeFileSync(path, bytes);
      console.log(`${folder}/${name}: ${(bytes.length / 1024).toFixed(1)} KiB`);
    }
  }
}
if (stale) {
  console.error(
    "The embedded pages differ from their sources: run `npm run build` in web/ and commit the result.",
  );
  process.exit(1);
}
