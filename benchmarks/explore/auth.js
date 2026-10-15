// Access tokens of every shape, against a server authenticating with an HMAC secret
// (`AUTH_JWT_SECRET`): valid tokens carrying hostile claims (long, Unicode, nested, quoting,
// claim names that look like settings) must be served, and invalid ones (expired, for another
// audience or issuer, unsigned, tampered, malformed, NUL characters PostgreSQL cannot hold)
// refused with 401 or 400. Never a 5xx, and never an answer of the other kind.
import http from "k6/http";
import crypto from "k6/crypto";
import encoding from "k6/encoding";
import { Counter } from "k6/metrics";
import { BASE, DURATION, engineFailures, GRAPHQL, VUS } from "./lib/graphpg.js";

const SECRET = __ENV.AUTH_SECRET || "";
const ISSUER = __ENV.AUTH_ISSUER || "";
const AUDIENCE = __ENV.AUTH_AUDIENCE || "";

/** Tokens answered with the wrong kind of answer (a valid one refused, an invalid one served). */
const mismatches = new Counter("auth_mismatches");

export const options = {
  scenarios: { tokens: { executor: "constant-vus", vus: VUS, duration: DURATION } },
  thresholds: { engine_failures: ["count==0"], auth_mismatches: ["count==0"] },
};

const b64 = (value) => encoding.b64encode(typeof value === "string" ? value : JSON.stringify(value), "rawurl");

function sign(header, claims, secret = SECRET) {
  const unsigned = `${b64(header)}.${b64(claims)}`;
  return `${unsigned}.${crypto.hmac("sha256", secret, unsigned, "base64rawurl")}`;
}

const now = () => Math.floor(Date.now() / 1000);
const base = () => ({ iss: ISSUER, aud: AUDIENCE, sub: `user-${__VU}`, exp: now() + 300, iat: now() });

const HOSTILE = [
  "x".repeat(10000),
  "ñandú 𝄞 😀 ‮",
  `'"; DROP TABLE app_public.forums; --`,
  "$1 $$ \\ \\\\ %s %n {}",
  { nested: { deeper: [1, "two", null, { three: true }] } },
  [1, 2, 3],
  1e308,
  -0,
  true,
  null,
  "",
];

/** A valid token: claims of every shape, under names that are and are not identifiers. */
function validToken() {
  const claims = base();
  const value = HOSTILE[Math.floor(Math.random() * HOSTILE.length)];
  const name = ["role", "tenant", "claims", "request", "https://example.com/roles", "with.dots", "UPPER", "_x"][
    Math.floor(Math.random() * 8)
  ];
  claims[name] = value;
  claims.org = { id: value };
  return sign({ alg: "HS256", typ: "JWT" }, claims);
}

const INVALID = [
  () => sign({ alg: "HS256" }, { ...base(), exp: now() - 3600 }), // expired
  () => sign({ alg: "HS256" }, { ...base(), nbf: now() + 3600 }), // not yet valid
  () => sign({ alg: "HS256" }, { ...base(), aud: "someone-else" }),
  () => sign({ alg: "HS256" }, { ...base(), iss: "https://evil.example/" }),
  () => sign({ alg: "HS256" }, base(), "another-secret-of-at-least-thirty-two-bytes"),
  () => `${b64({ alg: "none" })}.${b64(base())}.`, // unsigned
  () => sign({ alg: "HS256" }, base()).replace(/.$/, (c) => (c === "A" ? "B" : "A")), // tampered
  () => sign({ alg: "HS256" }, { ...base(), sub: "nul\u0000byte" }), // a claim PostgreSQL cannot hold
  () => sign({ alg: "HS256" }, { ...base(), "nul\u0000name": 1 }),
  () => sign({ alg: "HS512" }, base()), // an algorithm not accepted
  () => "not-a-token",
  () => "a.b.c",
  () => "..",
  () => `${b64({ alg: "HS256" })}.${"x".repeat(20000)}.sig`,
];

function check(kind, res, path) {
  if (res.status >= 500) {
    engineFailures.add(1);
    console.error(`FAIL ${kind} ${path} (${res.status}): ${String(res.body).slice(0, 300)}`);
  } else if ((kind === "valid") !== (res.status === 200)) {
    mismatches.add(1);
    console.error(`MISMATCH ${kind} ${path} (${res.status}): ${String(res.body).slice(0, 300)}`);
  }
}

/** What the requests read, from the database: `QUERY` (GraphQL) and `RESOURCE` (REST). */
const QUERY = __ENV.QUERY || "";
const RESOURCE = __ENV.RESOURCE || "";

export function setup() {
  if (!QUERY || !RESOURCE) throw new Error("set QUERY (a GraphQL query) and RESOURCE (a REST resource)");
}

export default function () {
  const valid = Math.random() < 0.5;
  const token = valid ? validToken() : INVALID[Math.floor(Math.random() * INVALID.length)]();
  const headers = { "content-type": "application/json", authorization: `Bearer ${token}` };
  const kind = valid ? "valid" : "invalid";
  // Requests that reach the database, where the claims become settings
  check(kind, http.post(GRAPHQL, JSON.stringify({ query: QUERY }), { headers }), "/graphql");
  check(kind, http.get(`${BASE}/rest/${RESOURCE}?first=1`, { headers }), `/rest/${RESOURCE}`);
}
