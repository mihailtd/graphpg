//! Scalars: every PostgreSQL type has a GraphQL type whose values round-trip. Inputs are
//! checked before any SQL runs, outputs follow the scalar's format (non-finite floats are
//! null, `money` a decimal, `bytea` base64, `interval` an ISO 8601 duration), ranges are
//! objects, and domains are scalars of their own.
//!
//! Uses a `scalar_fixture` schema created on demand (recreated when `FIXTURE_VERSION` changes).
mod common;
use graphpg::catalog::CatalogInspector;
use graphpg::config::EngineConfig;
use graphpg::execution::ExecutionEngine;
use graphpg::planner::QueryPlanner;
use graphpg::schema::SchemaBuilder;
use graphpg::DatabaseCatalog;
use serde_json::json;
use tokio_postgres::NoTls;

const SCHEMA: &str = "scalar_fixture";
const FIXTURE_VERSION: &str = "scalar_fixture v1";

const FIXTURE_SQL: &str = r#"
CREATE SCHEMA scalar_fixture;
SET search_path = scalar_fixture, public;

CREATE DOMAIN email AS citext CHECK (value ~ '@');
COMMENT ON DOMAIN email IS 'An email address';
CREATE DOMAIN positive_int AS int CHECK (value > 0);

CREATE TABLE samples (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    label text NOT NULL UNIQUE,
    n bigint, d numeric, f float8, g float8, m money,
    ts timestamp, tz timestamptz, dt date, tm time, ttz timetz, iv interval,
    ip inet, net cidr, mac macaddr, b bytea, emb vector(3),
    r int4range, period tstzrange, amounts numrange,
    ips inet[], ivs interval[],
    email email, qty positive_int
);
INSERT INTO samples (label, n, d, f, g, m, ts, tz, dt, tm, ttz, iv, ip, net, mac, b, emb, r, period, amounts, ips, ivs, email, qty) VALUES
    ('full', 9007199254740993, 1.50, 'NaN', 'Infinity', 12.5, '2024-05-01 12:30', '2024-05-01 12:30+02', '2024-05-01', '12:30',
     '12:30+02', '1 year 2 months -3 days 04:05:06.5', '10.0.0.1/24', '10.0.0.0/24', '08:00:2b:01:02:03', '\xdeadbeef', '[1,0,0]',
     '[1,5)', '[2024-01-01T00:00:00Z,)', 'empty', '{10.0.0.1,::1}', '{"1 day","0"}', 'Ada@Example.org', 3),
    ('bare', NULL, NULL, 2.5, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '0', NULL, NULL, NULL, NULL, NULL,
     '(,10]', NULL, NULL, NULL, NULL, NULL, NULL);
"#;

async fn connect() -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(common::db_url(), NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("connection error: {}", e);
        }
    });
    client
}

/// Creates the fixture schema once (serialised by an advisory lock, since tests run
/// concurrently), recreating it when the fixture changed.
async fn ensure_fixture(client: &tokio_postgres::Client) {
    client
        .execute("SELECT pg_advisory_lock(424245)", &[])
        .await
        .unwrap();
    let version: Option<String> = client
        .query_opt(
            "SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = $1",
            &[&SCHEMA],
        )
        .await
        .unwrap()
        .and_then(|row| row.get(0));
    if version.as_deref() != Some(FIXTURE_VERSION) {
        client
            .batch_execute(&format!(
                "BEGIN; DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; {FIXTURE_SQL} COMMENT ON SCHEMA {SCHEMA} IS '{FIXTURE_VERSION}'; COMMIT;"
            ))
            .await
            .expect("fixture setup failed");
    }
    client
        .execute("SELECT pg_advisory_unlock(424245)", &[])
        .await
        .unwrap();
}

struct Env {
    client: tokio_postgres::Client,
    schema: apollo_compiler::validation::Valid<apollo_compiler::Schema>,
    catalog: DatabaseCatalog,
    sdl: String,
}

async fn setup_with(config: &str) -> Env {
    let client = connect().await;
    ensure_fixture(&client).await;
    let config = EngineConfig::from_yaml(config).expect("config parses");
    let catalog = CatalogInspector::new(&client)
        .inspect_schema_with_config(SCHEMA, &config)
        .await
        .expect("inspection failed");
    let sdl = SchemaBuilder::generate_sdl(&catalog);
    let schema =
        SchemaBuilder::build(&catalog).unwrap_or_else(|e| panic!("invalid schema: {e}\n{sdl}"));
    Env {
        client,
        schema,
        catalog,
        sdl,
    }
}

async fn setup() -> Env {
    setup_with("").await
}

impl Env {
    async fn try_run(&self, query: &str) -> Result<serde_json::Value, String> {
        let doc = apollo_compiler::ExecutableDocument::parse_and_validate(
            &self.schema,
            query,
            "q.graphql",
        )
        .map_err(|e| format!("validation: {e}"))?;
        let plan = QueryPlanner::new(&self.catalog, SCHEMA)
            .plan(&doc)
            .map_err(|e| format!("planning: {e}"))?;
        let result = ExecutionEngine::execute(&self.client, &plan)
            .await
            .map_err(|e| format!("execution: {e}"))?;
        Ok(result["data"].clone())
    }

    async fn run(&self, query: &str) -> serde_json::Value {
        self.try_run(query)
            .await
            .unwrap_or_else(|e| panic!("{e}\nquery: {query}"))
    }

    /// Block of `keyword name` in the SDL, with its last line's newline.
    fn block(&self, keyword: &str, name: &str) -> String {
        let start = self
            .sdl
            .find(&format!("{keyword} {name} "))
            .unwrap_or_else(|| panic!("no {keyword} {name} in SDL:\n{}", self.sdl));
        let end = self.sdl[start..]
            .find("\n}")
            .map(|e| start + e + 1)
            .unwrap_or(self.sdl.len());
        self.sdl[start..end].to_string()
    }
}

const RANGE: &str = "{ start { value inclusive } end { value inclusive } isEmpty }";

#[tokio::test]
async fn test_every_type_has_a_graphql_type() {
    let env = setup().await;
    let sample = env.block("type", "Sample");
    for field in [
        "n: BigInt",
        "d: Decimal",
        "f: Float",
        "m: Decimal",
        "ts: LocalDatetime",
        "tz: Datetime",
        "dt: Date",
        "tm: Time",
        "ttz: TimeTz",
        "iv: Interval",
        "ip: InternetAddress",
        "net: CidrAddress",
        "mac: MacAddress",
        "b: Base64",
        "emb: [Float!]",
        "r: IntRange",
        "period: DatetimeRange",
        "amounts: DecimalRange",
        "ips: [InternetAddress]",
        "ivs: [Interval]",
        "email: Email",
        "qty: PositiveInt",
    ] {
        assert!(
            sample.contains(&format!("  {field}\n")),
            "{field}\n{sample}"
        );
    }
    assert!(env.sdl.contains("\"\"\"An email address\n\nValues of domain `email`, as `String` values.\"\"\"\nscalar Email"), "{}", env.sdl);
    assert!(env.sdl.contains("scalar PositiveInt"));

    let filter = env.block("input", "SampleFilter");
    for field in [
        "ts: LocalDatetimeFilter",
        "iv: IntervalFilter",
        "ip: InternetAddressFilter",
        "m: DecimalFilter",
        "r: IntRangeFilter",
        "email: StringFilter",
    ] {
        assert!(
            filter.contains(&format!("  {field}\n")),
            "{field}\n{filter}"
        );
    }
    assert!(
        !filter.contains("  b:") && !filter.contains("  emb:"),
        "binary data and vectors are not filtered: {filter}"
    );
    assert!(env
        .block("input", "SampleCondition")
        .contains("  r: IntRangeInput\n"));
    let create = env.block("input", "CreateSampleInput");
    assert!(
        create.contains("  period: DatetimeRangeInput\n")
            && create.contains("  b: Base64\n")
            && create.contains("  email: Email\n"),
        "{create}"
    );
    assert!(env
        .block("input", "IntRangeFilter")
        .contains("  contains: Int\n"));
}

#[tokio::test]
async fn test_values_follow_their_scalar() {
    let env = setup().await;
    let data = env
        .run(&format!(
            r#"{{ full: sampleByLabel(label: "full") {{ n d f g m ts tz dt tm ttz iv ip net mac b emb ips ivs email qty
                    r {RANGE} period {RANGE} amounts {RANGE} }}
                bare: sampleByLabel(label: "bare") {{ n f iv r {RANGE} }}
                samplesConnection {{ aggregates {{ min {{ iv }} max {{ iv }} }} }} }}"#
        ))
        .await;
    assert_eq!(
        data["full"],
        json!({
            "n": "9007199254740993", "d": "1.50", "f": null, "g": null, "m": "12.50",
            "ts": "2024-05-01T12:30:00", "tz": "2024-05-01T10:30:00+00:00", "dt": "2024-05-01", "tm": "12:30:00", "ttz": "12:30:00+02",
            "iv": "P1Y2M-3DT4H5M6.5S", "ip": "10.0.0.1/24", "net": "10.0.0.0/24", "mac": "08:00:2b:01:02:03", "b": "3q2+7w==",
            "emb": [1, 0, 0], "ips": ["10.0.0.1", "::1"], "ivs": ["P1D", "PT0S"], "email": "Ada@Example.org", "qty": 3,
            "r": { "start": { "value": 1, "inclusive": true }, "end": { "value": 5, "inclusive": false }, "isEmpty": false },
            "period": { "start": { "value": "2024-01-01T00:00:00+00:00", "inclusive": true }, "end": null, "isEmpty": false },
            "amounts": { "start": null, "end": null, "isEmpty": true }
        })
    );
    assert_eq!(
        data["bare"],
        json!({
            "n": null, "f": 2.5, "iv": "PT0S",
            "r": { "start": null, "end": { "value": 11, "inclusive": false }, "isEmpty": false }
        })
    );
    assert_eq!(
        data["samplesConnection"]["aggregates"],
        json!({ "min": { "iv": "PT0S" }, "max": { "iv": "P1Y2M-3DT4H5M6.5S" } })
    );
}

#[tokio::test]
async fn test_inputs_round_trip() {
    let env = setup().await;
    env.client.batch_execute("BEGIN").await.unwrap();
    let created = env
        .run(&format!(
            r#"mutation {{ createSample(input: {{
                label: "new", n: 9007199254740993, d: "123.456", m: "7.25", f: 1.5,
                ts: "2024-05-01T12:30:00", tz: "2024-05-01T12:30:00Z", dt: "2024-05-01", tm: "08:15", ttz: "08:15:00-05:30",
                iv: "P1DT2H30M", ip: "::1", net: "192.168.0.0/16", mac: "08-00-2b-01-02-03", b: "aGVsbG8=", emb: [0.5, 1, 2],
                r: {{ start: {{ value: 2 }}, end: {{ value: 8, inclusive: true }} }},
                period: {{ end: {{ value: "2025-01-01T00:00:00+00:00" }} }},
                ips: ["10.1.1.1"], ivs: ["PT1S"], email: "a@b.c", qty: 7
            }}) {{ sample {{ n d m f ts tz dt tm ttz iv ip net mac b emb ips ivs email qty r {RANGE} period {RANGE} }} }} }}"#
        ))
        .await;
    env.client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(
        created["createSample"]["sample"],
        json!({
            "n": "9007199254740993", "d": "123.456", "m": "7.25", "f": 1.5,
            "ts": "2024-05-01T12:30:00", "tz": "2024-05-01T12:30:00+00:00", "dt": "2024-05-01", "tm": "08:15:00", "ttz": "08:15:00-05:30",
            "iv": "P1DT2H30M", "ip": "::1", "net": "192.168.0.0/16", "mac": "08:00:2b:01:02:03", "b": "aGVsbG8=", "emb": [0.5, 1, 2],
            "ips": ["10.1.1.1"], "ivs": ["PT1S"], "email": "a@b.c", "qty": 7,
            // PostgreSQL normalises discrete ranges to [start, end)
            "r": { "start": { "value": 2, "inclusive": true }, "end": { "value": 9, "inclusive": false }, "isEmpty": false },
            "period": { "start": null, "end": { "value": "2025-01-01T00:00:00+00:00", "inclusive": false }, "isEmpty": false }
        })
    );
}

#[tokio::test]
async fn test_filters_on_new_types_and_ranges() {
    let env = setup().await;
    let labels = |data: &serde_json::Value, key: &str| -> Vec<String> {
        let mut v: Vec<String> = data[key]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["label"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let data = env
        .run(
            r#"{
                contains: samplesConnection(filter: { r: { contains: 3 } }) { nodes { label } }
                containsEdge: samplesConnection(filter: { r: { contains: 10 } }) { nodes { label } }
                overlaps: samplesConnection(filter: { r: { overlaps: { start: { value: 4 }, end: { value: 6 } } } }) { nodes { label } }
                leftOf: samplesConnection(filter: { r: { strictlyLeftOf: { start: { value: 20 } } } }) { nodes { label } }
                longer: samplesConnection(filter: { iv: { greaterThan: "PT1H" } }) { nodes { label } }
                ip: samplesConnection(filter: { ip: { equalTo: "10.0.0.1/24" } }) { nodes { label } }
                condition: samplesConnection(condition: { r: { start: { value: 1 }, end: { value: 5 } } }) { nodes { label } }
                emailCi: samplesConnection(filter: { email: { equalTo: "ada@example.org" } }) { nodes { label } }
            }"#,
        )
        .await;
    assert_eq!(labels(&data, "contains"), ["bare", "full"]);
    assert_eq!(labels(&data, "containsEdge"), ["bare"]);
    assert_eq!(labels(&data, "overlaps"), ["bare", "full"]);
    assert_eq!(labels(&data, "leftOf"), ["bare", "full"]);
    assert_eq!(labels(&data, "longer"), ["full"]);
    assert_eq!(labels(&data, "ip"), ["full"]);
    assert_eq!(data["condition"]["nodes"], json!([{ "label": "full" }]));
    assert_eq!(
        labels(&data, "emailCi"),
        ["full"],
        "citext domain compares case-insensitively"
    );
}

#[tokio::test]
async fn test_invalid_inputs_are_rejected_before_any_sql() {
    let env = setup().await;
    let err = env
        .try_run(
            r#"mutation { createSample(input: {
                label: "bad", id: "not-a-uuid", n: 12.7, d: "abc", tz: "yesterday", dt: "2024-13-01", iv: "1 day",
                b: "***", ip: "10.0.0.300", net: "10.0.0.1/24", mac: "zz", qty: "many", ttz: "12:30",
                r: { start: { value: 1 }, end: { value: 5 } }
            }) { sample { label } } }"#,
        )
        .await
        .unwrap_err();
    for expected in [
        "`input.id`: \"not-a-uuid\" is not a UUID",
        "`input.n`: 12.7 is not a BigInt",
        "`input.d`: \"abc\" is not a Decimal",
        "`input.tz`: \"yesterday\" is not a Datetime",
        "`input.dt`: \"2024-13-01\" is not a Date",
        "`input.iv`: \"1 day\" is not an Interval",
        "`input.b`: \"***\" is not Base64",
        "`input.ip`: \"10.0.0.300\" is not an InternetAddress",
        "`input.net`: \"10.0.0.1/24\" is not a CidrAddress",
        "`input.mac`: \"zz\" is not a MacAddress",
        "`input.qty`: \"many\" is not an Int",
        "`input.ttz`: \"12:30\" is not a TimeTz",
    ] {
        assert!(
            err.starts_with("planning:") && err.contains(expected),
            "expected `{expected}` in: {err}"
        );
    }
    // Range bounds and filter operands are checked too
    let err = env.try_run(r#"{ samplesConnection(filter: { period: { contains: "soon" }, r: { overlaps: { start: { value: 1 } } } }) { nodes { label } } }"#).await.unwrap_err();
    assert!(
        err.contains("`period`: \"soon\" is not a Datetime"),
        "{err}"
    );
    let err = env.try_run(r#"{ samplesConnection(filter: { tz: { greaterThan: "later" } }) { nodes { label } } }"#).await.unwrap_err();
    assert!(err.contains("`tz`: \"later\" is not a Datetime"), "{err}");
    let count = env.run("{ samplesConnection { totalCount } }").await;
    assert_eq!(
        count["samplesConnection"]["totalCount"], 2,
        "nothing was written"
    );
}

#[tokio::test]
async fn test_config_names_domain_scalars() {
    let env =
        setup_with("types:\n  email: { type_name: EmailAddress, description: Where mail goes }\n")
            .await;
    assert!(env
        .block("type", "Sample")
        .contains("  email: EmailAddress\n"));
    assert!(env.sdl.contains("\"\"\"Where mail goes\n\nValues of domain `email`, as `String` values.\"\"\"\nscalar EmailAddress"));
    assert!(!env.sdl.contains("scalar Email\n"));
}
