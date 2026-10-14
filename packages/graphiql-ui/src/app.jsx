import "./style.css";
import { explorerPlugin } from "@graphiql/plugin-explorer";
import { createGraphiQLFetcher } from "@graphiql/toolkit";
import { GraphiQL, HISTORY_PLUGIN } from "graphiql";
import { parse } from "graphql";
import React from "react";
import ReactDOM from "react-dom/client";

// Configure Monaco Editor to fetch worker scripts from the local server
window.MonacoEnvironment = {
  getWorkerUrl: (_moduleId, label) => {
    if (label === "json") return "/graphiql/json.worker.js";
    if (label === "graphql") return "/graphiql/graphql.worker.js";
    return "/graphiql/editor.worker.js";
  },
};

const h = React.createElement;

// --- Debug extensions store ---------------------------------------------------
// Holds `extensions` (sql / explain) from the latest non-introspection response,
// which the server includes when started with --debug / --debug-explain.
let lastDebug = null;
const listeners = new Set();
const debugStore = {
  get: () => lastDebug,
  set(value) {
    lastDebug = value;
    for (const listener of listeners) {
      listener();
    }
  },
  subscribe(listener) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
};

async function debugFetch(url, init) {
  const response = await fetch(url, init);
  let operationName = null;
  try {
    operationName = JSON.parse(init?.body ?? "{}").operationName ?? null;
  } catch {}
  if (operationName !== "IntrospectionQuery") {
    response
      .clone()
      .json()
      .then((json) => debugStore.set({ at: new Date(), extensions: json?.extensions ?? null }))
      .catch(() => {});
  }
  return response;
}

const httpFetcher = createGraphiQLFetcher({ url: "/graphql", fetch: debugFetch });

// --- Subscriptions (live queries) over GraphQL over SSE --------------------------------
// With `subscriptions.enabled`, a subscription's results arrive as SSE `next` events until
// GraphiQL stops it (then the request is cancelled and the server ends the subscription).
function isSubscription({ query, operationName }) {
  try {
    const operations = parse(query).definitions.filter((d) => d.kind === "OperationDefinition");
    const operation = operationName ? operations.find((d) => d.name?.value === operationName) : operations[0];
    return operation?.operation === "subscription";
  } catch {
    return false;
  }
}

// GraphiQL stops a subscription with `return()`, which an async generator only sees at its
// next `yield`: an unchanged result yields nothing, so the request is aborted instead.
function sseSubscription(params) {
  const controller = new AbortController();
  const events = sseEvents(params, controller.signal);
  return {
    [Symbol.asyncIterator]() {
      return this;
    },
    next: () => events.next(),
    return: () => {
      controller.abort();
      return Promise.resolve({ done: true, value: undefined });
    },
  };
}

async function* sseEvents(params, signal) {
  let response;
  try {
    response = await fetch("/graphql", {
      method: "POST",
      headers: { "content-type": "application/json", accept: "text/event-stream" },
      body: JSON.stringify(params),
      signal,
    });
  } catch (error) {
    if (signal.aborted) return;
    throw error;
  }
  if (!response.headers.get("content-type")?.startsWith("text/event-stream")) {
    yield await response.json();
    return;
  }
  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = "";
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      buffer += value;
      let end = buffer.indexOf("\n\n");
      while (end >= 0) {
        const block = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        let event = "";
        let data = "";
        for (const line of block.split("\n")) {
          if (line.startsWith("event: ")) event = line.slice(7);
          else if (line.startsWith("data: ")) data += line.slice(6);
        }
        if (event === "next") yield JSON.parse(data);
        if (event === "complete") return;
        end = buffer.indexOf("\n\n");
      }
    }
  } catch (error) {
    if (!signal.aborted) throw error;
  } finally {
    reader.cancel().catch(() => {});
  }
}

const fetcher = (params, options) => (isSubscription(params) ? sseSubscription(params) : httpFetcher(params, options));

// --- SQL plugin ------------------------------------------------------------------
function CopyButton({ text }) {
  const [copied, setCopied] = React.useState(false);
  return h(
    "button",
    {
      type: "button",
      onClick: () =>
        navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        }),
    },
    copied ? "Copied" : "Copy",
  );
}

function Section({ title, text }) {
  return h(React.Fragment, null, h("h3", null, title, h(CopyButton, { text })), h("pre", null, text));
}

function SqlPanel() {
  const state = React.useSyncExternalStore(debugStore.subscribe, debugStore.get);
  const ext = state?.extensions;

  let body;
  if (!state) {
    body = h("p", { className: "hint" }, "Run a query to see the SQL it compiles to.");
  } else if (!ext?.sql) {
    body = h(
      "p",
      { className: "hint" },
      "The last response had no SQL debug info. Start the server with ",
      h("code", null, "--debug"),
      " (SQL) or ",
      h("code", null, "--debug-explain"),
      " (SQL + EXPLAIN ANALYZE). Introspection-only queries never hit the database.",
    );
  } else {
    const params = ext.sql.params ?? [];
    const explain = ext.explain;
    body = h(
      React.Fragment,
      null,
      h(Section, { title: "SQL", text: ext.sql.query }),
      params.length > 0 &&
        h(Section, {
          title: "Parameters",
          text: params.map((p, i) => `$${i + 1} = ${JSON.stringify(p)}`).join("\n"),
        }),
      explain &&
        h(Section, {
          title: "EXPLAIN ANALYZE",
          text: Array.isArray(explain) ? explain.join("\n") : JSON.stringify(explain, null, 2),
        }),
    );
  }

  return h(
    "div",
    { className: "sql-panel" },
    h("div", { className: "graphiql-doc-explorer-title" }, "SQL"),
    state && h("p", { className: "hint" }, `Last request: ${state.at.toLocaleTimeString()}`),
    body,
  );
}

function DatabaseIcon() {
  return h(
    "svg",
    {
      viewBox: "0 0 24 24",
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 1.5,
      strokeLinecap: "round",
      strokeLinejoin: "round",
      "aria-hidden": true,
    },
    h("ellipse", { cx: 12, cy: 5, rx: 8, ry: 3 }),
    h("path", { d: "M4 5v14c0 1.66 3.58 3 8 3s8-1.34 8-3V5" }),
    h("path", { d: "M4 12c0 1.66 3.58 3 8 3s8-1.34 8-3" }),
  );
}

const sqlPlugin = { title: "SQL", icon: DatabaseIcon, content: SqlPanel };
const plugins = [HISTORY_PLUGIN, explorerPlugin(), sqlPlugin];

function App() {
  return h(GraphiQL, {
    fetcher,
    plugins,
    defaultEditorToolsVisibility: true,
  });
}

const container = document.getElementById("graphiql");
if (container) {
  const root = ReactDOM.createRoot(container);
  root.render(h(App));
}
