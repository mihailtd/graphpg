/**
 * `pnpm explore <graphql url> [--only <regex>] [--max <n>]`: crawls and fuzzes a running graphpg
 * (see `explore.ts`) and prints what it failed on. Exits with 1 when it failed on anything.
 */
import { crawl, type Failure, fuzz } from "./explore.ts";

const args = process.argv.slice(2);
const url = args.find((a) => a.startsWith("http"));
if (!url) {
  console.error("usage: pnpm explore <graphql url> [--only <regex>] [--max <n>]");
  process.exit(2);
}
const flag = (name: string) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
const only = flag("--only");
const max = flag("--max");
const options = { only: only ? new RegExp(only) : undefined, maxRequests: max ? Number(max) : undefined };

const report = (name: string, run: { requests: number; failures: Failure[] }) => {
  console.log(`${name}: ${run.requests} requests, ${run.failures.length} failures`);
  for (const f of run.failures.slice(0, 50)) {
    console.log(`  ${f.label} (${f.status}): ${JSON.stringify(f.error).slice(0, 300)}`);
  }
};
const crawled = await crawl(url, options);
report("crawl", crawled);
const fuzzed = await fuzz(url, options);
report("fuzz", fuzzed);
process.exit(crawled.failures.length + fuzzed.failures.length > 0 ? 1 : 0);
