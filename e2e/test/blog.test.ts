import { describe, expect, inject, it } from "vitest";

async function gql<T>(query: string, variables?: object): Promise<T> {
  const res = await fetch(inject("graphqlUrl"), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  const body = await res.json();
  expect(body.errors, JSON.stringify(body.errors)).toBeUndefined();
  return body.data;
}

describe("the blog schema, migrated by Drizzle and served by graphpg", () => {
  it("serves tables and relations", async () => {
    const data = await gql<{ authors: { username: string; postsConnection: { totalCount: number } }[] }>(
      `{ authors { username postsConnection { totalCount } } }`,
    );
    const byName = Object.fromEntries(data.authors.map((a) => [a.username, a.postsConnection.totalCount]));
    expect(byName).toEqual({ ada: 1, grace: 2 });
  });

  it("exposes a generated column and a row function as fields", async () => {
    const data = await gql<{ postsConnection: { nodes: unknown[] } }>(
      `{ postsConnection(filter: { title: { equalTo: "Nanoseconds" } }) { nodes { wordCount excerpt(length: 11) } } }`,
    );
    expect(data.postsConnection.nodes).toEqual([{ wordCount: 11, excerpt: "A nanosecon" }]);
  });

  it("serves many-to-many relations and STABLE functions as root fields", async () => {
    const data = await gql<{ popularTags: { nodes: { name: string }[] } }>(`{ popularTags { nodes { name } } }`);
    expect(data.popularTags.nodes.map((t) => t.name)).toEqual(["computing", "history"]);
  });

  it("enforces the Drizzle-defined constraints through mutations", async () => {
    const res = await fetch(inject("graphqlUrl"), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        query: `mutation { createComment(input: { postId: "00000000-0000-7000-8000-000000000000", authorId: "00000000-0000-7000-8000-000000000000", body: "" }) { id } }`,
      }),
    });
    expect((await res.json()).errors).toBeDefined();
  });

  it("serves tables over the REST API at /rest/<table_name>", async () => {
    const restUrl = inject("graphqlUrl").replace("/graphql", "/rest/authors");
    const res = await fetch(restUrl);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("application/json");
    const rows = (await res.json()) as Array<{ username: string }>;
    expect(Array.isArray(rows)).toBe(true);
    const usernames = rows.map((r) => r.username);
    expect(usernames).toContain("ada");
    expect(usernames).toContain("grace");
  });
});
