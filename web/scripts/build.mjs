// Builds what `graphrt serve --chat` serves: the chat page and the widget
// script, bundled from the packages here into crates/graphrt-cli/assets/chat,
// where the Rust build embeds them. The results are committed, so building
// graphrt needs no Node.js.
//
// Usage: node scripts/build.mjs [--check]
//   --check  build in memory and fail when the committed files differ
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = resolve(root, "..", "crates", "graphrt-cli", "assets", "chat");
const check = process.argv.includes("--check");

const banner =
  "/* graphrt chat, built from web/ of https://github.com/Graphrt/graphrt (Apache-2.0). Includes zod (MIT, Colin McDonnell). */";

const common = {
  bundle: true,
  minify: true,
  write: false,
  target: ["es2022", "chrome109", "firefox115", "safari16"],
  legalComments: "none",
  banner: { js: banner },
  logLevel: "warning",
};

const page = `<!doctype html>
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

async function bundle(entry, format) {
  const result = await build({
    ...common,
    entryPoints: [join(root, "packages", "chat", "src", entry)],
    format,
  });
  const [file] = result.outputFiles;
  if (file === undefined) {
    throw new Error(`esbuild produced nothing for ${entry}`);
  }
  return file.text;
}

const files = {
  "index.html": page,
  // A module: the page's policy allows scripts of its own origin only.
  "chat.js": await bundle("page.ts", "esm"),
  // A classic script: it reads `document.currentScript` to find its server and options.
  "embed.js": await bundle("embed.ts", "iife"),
};

let stale = false;
mkdirSync(outDir, { recursive: true });
for (const [name, text] of Object.entries(files)) {
  const path = join(outDir, name);
  if (check) {
    let committed = "";
    try {
      committed = readFileSync(path, "utf8");
    } catch {
      // Missing counts as different.
    }
    if (committed !== text) {
      console.error(`stale: ${path}`);
      stale = true;
    }
  } else {
    writeFileSync(path, text);
    console.log(`${name}: ${(Buffer.byteLength(text) / 1024).toFixed(1)} KiB`);
  }
}
if (stale) {
  console.error(
    "The chat assets differ from their sources: run `npm run build` in web/ and commit the result.",
  );
  process.exit(1);
}
