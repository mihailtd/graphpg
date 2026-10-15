import { describe, expect, inject, it } from "vitest";
import { crawl, fuzz } from "../src/explore.ts";

// What the schema offers is requested and fuzzed, as the explorer does on any schema: the engine
// may refuse a request, never fail on one
describe("exploring the blog schema", () => {
  it("fails on nothing it serves: rows, orderings and pages, aggregates, groupings, relations, filters", async () => {
    const run = await crawl(inject("graphqlUrl"));
    expect(run.requests).toBeGreaterThan(100);
    expect(run.failures, JSON.stringify(run.failures.slice(0, 5), null, 1)).toEqual([]);
  }, 120_000);

  it("refuses malformed values of every filter operator, cursors and ids, and fails on none", async () => {
    const run = await fuzz(inject("graphqlUrl"));
    expect(run.requests).toBeGreaterThan(500);
    expect(run.failures, JSON.stringify(run.failures.slice(0, 5), null, 1)).toEqual([]);
  }, 300_000);
});
