import { drizzle } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import pg from "pg";
import * as schema from "./schema.ts";

export const databaseUrl =
  process.env.E2E_DATABASE_URL ??
  process.env.TEST_DATABASE_URL ??
  "postgres://postgres:postgres@localhost:5432/graphpg_test";

/** Drop everything a previous run left behind, apply the Drizzle migrations, and seed. */
export async function resetDatabase() {
  const pool = new pg.Pool({ connectionString: databaseUrl, max: 2 });
  try {
    await pool.query("DROP SCHEMA IF EXISTS blog, drizzle CASCADE");
    const db = drizzle(pool, { schema });
    await migrate(db, { migrationsFolder: new URL("../drizzle", import.meta.url).pathname });
    await seed(db);
  } finally {
    await pool.end();
  }
}

async function seed(db: ReturnType<typeof drizzle<typeof schema>>) {
  const { authors, posts, comments, tags, postTags } = schema;
  const [ada, grace] = await db
    .insert(authors)
    .values([
      { username: "ada", displayName: "Ada Lovelace" },
      { username: "grace", displayName: "Grace Hopper" },
    ])
    .returning();
  const day = 86_400_000;
  const [engine, nanos] = await db
    .insert(posts)
    .values([
      {
        authorId: ada.id,
        title: "Notes on the Analytical Engine",
        body: "The engine weaves algebraical patterns just as the Jacquard loom weaves flowers and leaves.",
        status: "published",
        publishedAt: new Date(Date.now() - 3 * day),
      },
      {
        authorId: grace.id,
        title: "Nanoseconds",
        body: "A nanosecond is the distance light travels in about thirty centimetres.",
        status: "published",
        publishedAt: new Date(Date.now() - day),
      },
      {
        authorId: grace.id,
        title: "Compilers, a draft",
        body: "Programs should be written in a language people can read.",
      },
    ])
    .returning();
  await db.insert(comments).values([
    { postId: engine.id, authorId: grace.id, body: "A delight to read." },
    { postId: nanos.id, authorId: ada.id, body: "Do you have a wire to show?" },
  ]);
  const [history, computing] = await db
    .insert(tags)
    .values([{ name: "history" }, { name: "computing" }])
    .returning();
  await db.insert(postTags).values([
    { postId: engine.id, tagId: history.id },
    { postId: engine.id, tagId: computing.id },
    { postId: nanos.id, tagId: computing.id },
  ]);
}
