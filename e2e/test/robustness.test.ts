import { describe, expect, inject, it } from "vitest";

// Found by running graphpg against a large schema (FHIR R5) and exploring it: each is checked
// here end to end, against the built binary
const graphqlUrl = () => inject("graphqlUrl");
const baseUrl = () => graphqlUrl().replace(/\/graphql$/, "");

async function post(query: string, variables?: object) {
  const res = await fetch(graphqlUrl(), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  return { status: res.status, body: await res.json() };
}

describe("found by exploring a large schema", () => {
  it("serves an index of the REST API at /rest", async () => {
    const res = await fetch(`${baseUrl()}/rest`);
    expect(res.status).toBe(200);
    const index = await res.json();
    const authors = index.resources.find((r: { name: string }) => r.name === "authors");
    expect(authors).toMatchObject({ path: "/rest/authors", type: "Author" });
  });

  it("aggregates the minimum and maximum of uuid columns", async () => {
    const { body } = await post("{ authorsConnection { aggregates { min { id } max { id } } } }");
    expect(body.errors).toBeUndefined();
    const { min, max } = body.data.authorsConnection.aggregates;
    expect(min.id < max.id).toBe(true);
  });

  it("counts the rows after a cursor, without reading a page", async () => {
    const page = await post("{ authorsConnection(first: 1) { pageInfo { endCursor } } }");
    const cursor = page.body.data.authorsConnection.pageInfo.endCursor;
    const { body } = await post("query($c: String) { authorsConnection(first: 1, after: $c) { totalCount } }", {
      c: cursor,
    });
    expect(body.errors).toBeUndefined();
    expect(body.data.authorsConnection.totalCount).toBe(2);
  });

  it("refuses cursors it did not issue, and text with a NUL character, as input", async () => {
    const cursor = await post(
      "query($c: String) { authorsConnection(first: 1, after: $c) { totalCount nodes { id } } }",
      {
        c: "garbage",
      },
    );
    expect(cursor.body.errors[0].message).toContain("Invalid cursor");
    const nul = await post(
      "query($v: String) { authorsConnection(filter: { username: { equalTo: $v } }) { totalCount } }",
      {
        v: "a\u0000b",
      },
    );
    expect(nul.body.errors[0].extensions.code).toBe("BAD_USER_INPUT");
    expect((await fetch(`${baseUrl()}/rest/authors?after=garbage`)).status).toBe(400);
    expect((await fetch(`${baseUrl()}/rest/authors?filter[username][eq]=a%00b`)).status).toBe(400);
  });
});

describe("live queries", () => {
  it("send a subscription's result over SSE, then each change", async () => {
    const controller = new AbortController();
    const res = await fetch(graphqlUrl(), {
      method: "POST",
      headers: { "content-type": "application/json", accept: "text/event-stream" },
      body: JSON.stringify({ query: `subscription { authorByUsername(username: "ada") { displayName } }` }),
      signal: controller.signal,
    });
    expect(res.headers.get("content-type")).toContain("text/event-stream");
    const reader = res.body?.pipeThrough(new TextDecoderStream()).getReader();
    if (!reader) throw new Error("no body");
    let buffer = "";
    const next = async (): Promise<{ data: { authorByUsername: { displayName: string } } }> => {
      for (;;) {
        const end = buffer.indexOf("\n\n");
        if (end >= 0) {
          const block = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          const data = block.split("\n").find((l) => l.startsWith("data: "));
          if (block.startsWith("event: next") && data) return JSON.parse(data.slice(6));
          continue;
        }
        const { value, done } = await reader.read();
        if (done) throw new Error("the stream ended");
        buffer += value;
      }
    };
    try {
      const first = await next();
      const original = first.data.authorByUsername.displayName;
      const renamed = `${original} (live)`;
      const update = await post(
        `mutation($n: String) { updateAuthorByUsername(username: "ada", input: { displayName: $n }) { author { displayName } } }`,
        { n: renamed },
      );
      expect(update.body.errors).toBeUndefined();
      expect((await next()).data.authorByUsername.displayName).toBe(renamed);
      await post(
        `mutation($n: String) { updateAuthorByUsername(username: "ada", input: { displayName: $n }) { author { displayName } } }`,
        { n: original },
      );
    } finally {
      controller.abort();
    }
  });

  it("speak graphql-transport-ws over a WebSocket", async () => {
    const socket = new WebSocket(graphqlUrl().replace(/^http/, "ws"), "graphql-transport-ws");
    const messages: { type: string; id?: string; payload?: { data?: unknown } }[] = [];
    const waitFor = (type: string) =>
      new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`no ${type}: ${JSON.stringify(messages)}`)), 5000);
        const poll = setInterval(() => {
          if (messages.some((m) => m.type === type)) {
            clearInterval(poll);
            clearTimeout(timer);
            resolve();
          }
        }, 20);
      });
    socket.onmessage = (e) => messages.push(JSON.parse(String(e.data)));
    await new Promise((resolve) => {
      socket.onopen = resolve;
    });
    socket.send(JSON.stringify({ type: "connection_init" }));
    await waitFor("connection_ack");
    socket.send(
      JSON.stringify({
        id: "1",
        type: "subscribe",
        payload: { query: "subscription { authorsConnection { totalCount } }" },
      }),
    );
    await waitFor("next");
    expect(messages.find((m) => m.type === "next")).toEqual({
      id: "1",
      type: "next",
      payload: { data: { authorsConnection: { totalCount: 2 } } },
    });
    socket.close();
  });
});
