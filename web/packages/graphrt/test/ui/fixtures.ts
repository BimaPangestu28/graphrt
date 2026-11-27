import { join, resolve } from "node:path";

// scripts/test.mjs names the repository: the tests run bundled from another directory.
const repoRoot = process.env.GRAPHRT_REPO_ROOT ?? resolve(process.cwd(), "..");

/** The conformance fixtures shared with the `graphrt-ui` crate. */
export const fixturesDir = join(repoRoot, "crates", "graphrt-ui", "fixtures");
