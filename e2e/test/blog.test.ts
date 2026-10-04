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

  it("supports REST filtering, ordering, pagination, and embedding", async () => {
    const baseUrl = inject("graphqlUrl").replace("/graphql", "/rest/authors");

    // 1. Filtering: filter[username][eq]=ada
    const filterRes = await fetch(`${baseUrl}?filter[username][eq]=ada`);
    expect(filterRes.status).toBe(200);
    const filterRows = (await filterRes.json()) as Array<{ username: string }>;
    expect(filterRows).toHaveLength(1);
    expect(filterRows[0].username).toBe("ada");

    // 2. Ordering: order=username:desc
    const orderRes = await fetch(`${baseUrl}?order=username:desc`);
    expect(orderRes.status).toBe(200);
    const orderRows = (await orderRes.json()) as Array<{ username: string }>;
    expect(orderRows.map((r) => r.username)).toEqual(["grace", "ada"]);

    // 3. Keyset Pagination & Link header: first=1&order=username:asc
    const page1Res = await fetch(`${baseUrl}?first=1&order=username:asc`);
    expect(page1Res.status).toBe(200);
    expect(page1Res.headers.get("x-total-count")).toBe("2");
    const linkHeader = page1Res.headers.get("link");
    expect(linkHeader).toBeDefined();
    expect(linkHeader).toContain('rel="next"');
    const page1Rows = (await page1Res.json()) as Array<{ username: string }>;
    expect(page1Rows).toHaveLength(1);
    expect(page1Rows[0].username).toBe("ada");

    // 4. Projection: fields=username
    const projRes = await fetch(`${baseUrl}?fields=username`);
    expect(projRes.status).toBe(200);
    const projRows = (await projRes.json()) as Array<Record<string, unknown>>;
    expect(projRows[0].username).toBeDefined();
    expect(projRows[0].id).toBeUndefined();

    // 5. Relation embedding: include=posts
    const embedRes = await fetch(`${baseUrl}?filter[username][eq]=ada&include=posts`);
    expect(embedRes.status).toBe(200);
    const embedRows = (await embedRes.json()) as Array<{ username: string; posts: Array<{ title: string }> }>;
    expect(embedRows).toHaveLength(1);
    expect(embedRows[0].username).toBe("ada");
    expect(Array.isArray(embedRows[0].posts)).toBe(true);

    // 6. Complex Search POST: /rest/authors/search
    const searchRes = await fetch(`${baseUrl}/search`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        filter: {
          username: { eq: "grace" },
        },
      }),
    });
    expect(searchRes.status).toBe(200);
    const searchRows = (await searchRes.json()) as Array<{ username: string }>;
    expect(searchRows).toHaveLength(1);
    expect(searchRows[0].username).toBe("grace");
  });
});
