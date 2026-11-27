#!/usr/bin/env bash
# Publishes the very first version of the npm package `@graphrt-dev/graphrt` by hand.
#
# npm lets a package name a trusted publisher (the release workflow) only once
# the package exists, so its first version is published once from the machine
# of an owner of the npm organization graphrt-dev. Every later version is
# published by the `npm` job of .github/workflows/release.yml. See
# web/README.md, "Publishing".
#
# It refuses to run unless `npm whoami` succeeds, refuses when
# `@graphrt-dev/graphrt` is on npm already, and builds from a fresh clone of the
# release tag, never from this working tree. The package is built, staged and
# checked by the tag's own scripts/publish.mjs (`--pack`), then the tarball is
# installed in a scratch project, each entry point is imported by package name
# in Node.js and bundled by esbuild, and that same tarball is published. npm
# confirms the publish in the browser (`--auth-type=web`, a passkey or a
# one-time password), or set NPM_CONFIG_OTP.
#
# Tags up to v0.6.3 still name the package `graphrt`, which npm refused as too
# close to `graphql`. `--rename` publishes such a tag as it is: after the tag's
# own checks pass, it rewrites only `name` in package.json and the import paths
# in README.md of the staged copy, and changes nothing else.
#
# Usage: web/scripts/first-publish.sh [--dry-run] [--rename] vX.Y.Z
#   --dry-run  do everything but publish: write the .tgz and print its path
#   --rename   the tag's package is still named `graphrt`: publish it as
#              `@graphrt-dev/graphrt`
# Environment: GRAPHRT_REPO_URL (default: https://github.com/Graphrt/graphrt.git)
set -euo pipefail

name="@graphrt-dev/graphrt"
old_name="graphrt"

usage() {
  echo "usage: web/scripts/first-publish.sh [--dry-run] [--rename] vX.Y.Z" >&2
  exit 2
}

fail() {
  echo "first-publish: $*" >&2
  exit 1
}

dry_run=false
rename=false
tag=""
for argument in "$@"; do
  case "$argument" in
    --dry-run) dry_run=true ;;
    --rename) rename=true ;;
    v[0-9]*) [ -z "$tag" ] || usage; tag="$argument" ;;
    *) usage ;;
  esac
done
[ -n "$tag" ] || usage
case "$tag" in
  *[!0-9A-Za-z.+-]*) echo "first-publish: '$tag' is not a release tag" >&2; exit 2 ;;
esac
version="${tag#v}"
repo_url="${GRAPHRT_REPO_URL:-https://github.com/Graphrt/graphrt.git}"

if ! user="$(npm whoami 2>/dev/null)" || [ -z "$user" ]; then
  fail "npm whoami failed. Run \`npm login\` with an owner of the npm organization graphrt-dev, then try again."
fi
echo "npm user: $user"

# The package must not exist yet: a 404 is the only answer that lets this run.
if view="$(npm view "$name" version 2>&1)"; then
  fail "$name is on npm already ($view). Later versions are published by the release workflow."
fi
case "$view" in
  *E404*|*"404 Not Found"*) ;;
  *) fail "cannot ask npm about $name: $view" ;;
esac
# The scope is an npm organization the user belongs to, or the publish fails.
if ! npm org ls graphrt-dev "$user" >/dev/null 2>&1; then
  if [ "$dry_run" = true ]; then
    echo "warning: $user is not a member of the npm organization graphrt-dev (or it does not exist): a real publish would fail" >&2
  else
    fail "$user is not a member of the npm organization graphrt-dev (or it does not exist). Create it on npmjs.com, Add Organization, then try again."
  fi
fi

if [ -z "$(git ls-remote --tags "$repo_url" "refs/tags/$tag")" ]; then
  fail "$tag is not a tag of $repo_url"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "cloning $tag of $repo_url"
git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" "$repo_url" "$work/graphrt"
checkout="$work/graphrt"
if [ "$(git -C "$checkout" describe --tags --exact-match HEAD)" != "$tag" ]; then
  fail "the clone is not at the tag $tag"
fi
manifest="$checkout/web/packages/graphrt/package.json"
[ -f "$manifest" ] || fail "$tag has no web/packages/graphrt: tag a release that has it"
if [ "$rename" = true ]; then
  grep -q "\"name\": \"$old_name\"" "$manifest" \
    || fail "$tag does not name the package $old_name: publish it without --rename"
  tag_name="$old_name"
else
  grep -q "\"name\": \"$name\"" "$manifest" \
    || fail "$tag names the package $(grep -m1 '"name"' "$manifest" | tr -d ' ,'), not $name: add --rename for a tag that still says $old_name"
  tag_name="$name"
fi

cd "$checkout/web"
npm ci --no-audit --no-fund
# The tag's own build and checks, under the name the tag gives the package.
node scripts/publish.mjs --pack --version "$version"
stage=".pack/node_modules/$tag_name"
[ -f "$stage/package.json" ] || fail "scripts/publish.mjs of $tag staged nothing in $stage"

if [ "$rename" = true ]; then
  echo "renaming the staged copy: $old_name -> $name (package.json name and README.md import paths)"
  node -e '
    const fs = require("node:fs");
    const [path, from, to] = process.argv.slice(1);
    const manifest = JSON.parse(fs.readFileSync(path, "utf8"));
    if (manifest.name !== from) {
      console.error(`first-publish: the staged package is named ${manifest.name}, not ${from}`);
      process.exit(1);
    }
    manifest.name = to;
    manifest.publishConfig = { ...manifest.publishConfig, access: "public" };
    fs.writeFileSync(path, `${JSON.stringify(manifest, null, 2)}\n`);
  ' "$stage/package.json" "$old_name" "$name"
  NAME="$name" perl -pi -e '
    s{^# graphrt$}{# $ENV{NAME}};
    s{npm install graphrt\b(?!-)}{npm install $ENV{NAME}}g;
    s{(?<![\w@/.-])graphrt/(client|ui|chat|monitor|console)\b}{$ENV{NAME}/$1}g;
  ' "$stage/README.md"
fi

# The entry points the tag publishes (v0.6.3 has no console yet).
parts="$(node -p 'Object.keys(require(process.argv[1]).exports).filter((p) => p !== "./package.json").map((p) => p.slice(2)).join(" ")' "$PWD/$stage/package.json")"
[ -n "$parts" ] || fail "the staged package exports nothing"
echo "entry points: $parts"

pack_dir="$PWD/.pack"
rm -f "$pack_dir"/*.tgz
tarball="$(cd "$stage" && npm pack --json --pack-destination "$pack_dir" | node -e 'let s = ""; process.stdin.on("data", (d) => { s += d; }).on("end", () => console.log(JSON.parse(s)[0].filename));')"
tarball="$pack_dir/$tarball"
[ -f "$tarball" ] || fail "npm pack wrote no tarball"

# The tarball as a user gets it: installed in an empty project, every entry
# point imported by package name, and nothing bundled for an import left unused.
scratch="$work/scratch"
mkdir -p "$scratch"
(
  cd "$scratch"
  npm init -y >/dev/null
  npm install --no-audit --no-fund "$tarball" >/dev/null
  installed="$(node -p 'require("./node_modules/@graphrt-dev/graphrt/package.json").name + "@" + require("./node_modules/@graphrt-dev/graphrt/package.json").version')"
  [ "$installed" = "$name@$version" ] || fail "the tarball installs as $installed, not $name@$version"
  for part in $parts; do
    node --input-type=module -e "const m = await import('$name/$part'); if (Object.keys(m).length === 0) process.exit(3);" \
      || fail "$name/$part does not load in Node.js"
  done
  unused=""
  for part in $parts; do
    unused="${unused}import \"$name/$part\";"
  done
  bundled="$(printf '%s' "$unused" | "$checkout/web/node_modules/.bin/esbuild" --bundle --minify --format=esm --platform=browser --log-level=error)"
  [ -z "$(printf '%s' "$bundled" | tr -d '[:space:]')" ] || fail "importing the parts without using them still bundles code"
  client="$(printf 'import { GraphrtClient } from "%s/client"; globalThis.client = GraphrtClient;' "$name" \
    | "$checkout/web/node_modules/.bin/esbuild" --bundle --minify --format=esm --platform=browser --log-level=error)"
  case "$client" in
    *customElements*|*mermaid*|*graphrt-block*|*--grt-*) fail "$name/client bundles UI code" ;;
  esac
)
echo "checked $(basename "$tarball"): installs, every entry point loads by name, tree shakes"

if [ "$dry_run" = true ]; then
  out="${TMPDIR:-/tmp}/graphrt-first-publish"
  mkdir -p "$out"
  cp "$tarball" "$out/"
  echo "dry run: nothing published, the package is $out/$(basename "$tarball")"
  exit 0
fi

publish=(publish "$tarball" --access public --auth-type=web)
case "$version" in
  *-*) publish+=(--tag next) ;;
esac
npm "${publish[@]}"
echo "published $name@$version"
echo "Next: on npmjs.com, $name, Settings, Trusted publishing, add GitHub Actions:"
echo "  organization Graphrt, repository graphrt, workflow release.yml, environment npm."
echo "Then set the repository variable PUBLISH_NPM to true."
