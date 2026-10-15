//! Exported schemas and the pinned-schema check.
//!
//! The server needs the database catalog at startup whatever else it has, so an exported schema
//! cannot replace introspection. It can *pin* it instead: CI exports the GraphQL SDL and the
//! OpenAPI document (`graphpg schema export`) and commits them; at startup, and in CI with
//! `graphpg schema export --check`, what the database generates now is compared with those
//! files. A difference is *drift*: in prod mode the server refuses to start (`export.pin:
//! fail`), in dev mode it warns (`warn`), with a summary of what changed.

use crate::catalog::DatabaseCatalog;
use crate::config::models::{Config, PinMode};
use crate::openapi::OpenApiGenerator;
use crate::schema::SchemaBuilder;
use apollo_compiler::validation::Valid;
use apollo_compiler::Schema;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::path::Path;

/// Most changed lines (or paths) a drift summary lists.
const SUMMARY_LINES: usize = 25;

/// Everything exported: what the database generates now.
pub struct Artifacts {
    /// The GraphQL SDL.
    pub graphql: String,
    /// The Apollo Federation subgraph SDL, when federation is on.
    pub subgraph: Option<String>,
    /// The OpenAPI 3.1 document.
    pub openapi: Value,
}

impl Artifacts {
    /// Generates the schemas of `catalog` as the server would serve them.
    pub fn build(
        catalog: &DatabaseCatalog,
        config: &Config,
        default_schema: &str,
        schema: &Valid<Schema>,
    ) -> Self {
        Self {
            graphql: SchemaBuilder::generate_sdl(catalog),
            subgraph: config
                .graphql
                .federation
                .enabled
                .then(|| SchemaBuilder::generate_federation_subgraph_sdl(catalog)),
            openapi: OpenApiGenerator::generate_with_schema(
                catalog,
                config,
                default_schema,
                Some(schema),
            ),
        }
    }

    /// Writes every artifact to the paths of `config.export`, returning them.
    pub fn write(&self, config: &Config) -> std::io::Result<Vec<String>> {
        let mut written = Vec::new();
        let mut put = |path: &str, content: &str| -> std::io::Result<()> {
            write_file(path, content)?;
            written.push(path.to_string());
            Ok(())
        };
        put(&config.export.graphql, &self.graphql)?;
        if let Some(subgraph) = &self.subgraph {
            put(&config.export.subgraph, subgraph)?;
        }
        put(
            &config.export.openapi,
            &render_openapi(&self.openapi, &config.export.openapi),
        )?;
        Ok(written)
    }
}

/// Writes `content` to `path`, creating its directory.
pub fn write_file(path: &str, content: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)
}

/// The OpenAPI document as the file at `path` holds it: YAML for `.yaml` / `.yml`, else JSON.
pub fn render_openapi(doc: &Value, path: &str) -> String {
    if is_yaml(path) {
        serde_yaml::to_string(doc).unwrap_or_default()
    } else {
        let mut json = serde_json::to_string_pretty(doc).unwrap_or_default();
        json.push('\n');
        json
    }
}

fn is_yaml(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".yaml") || path.ends_with(".yml")
}

/// What a pinned file says about the schema generated now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    /// The file matches.
    Match,
    /// There is no file: nothing is pinned.
    Missing,
    /// The file differs: what changed, a few lines of it.
    Drift(Drift),
    /// The file cannot be read or parsed.
    Unreadable(String),
}

/// A pinned artifact that differs from what the database generates now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    /// What changed, as `+` (new), `-` (gone) and `~` (changed) lines, at most [`SUMMARY_LINES`].
    pub changes: Vec<String>,
    /// How many more changes there are.
    pub more: usize,
}

/// One checked artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinResult {
    /// `graphql`, `subgraph` or `openapi`.
    pub artifact: &'static str,
    pub path: String,
    pub status: PinStatus,
}

impl PinResult {
    /// A report for people: the file, and what changed.
    pub fn report(&self) -> String {
        match &self.status {
            PinStatus::Match => format!("{} ({}): matches", self.artifact, self.path),
            PinStatus::Missing => format!("{} ({}): no pinned file", self.artifact, self.path),
            PinStatus::Unreadable(why) => {
                format!("{} ({}): cannot be read: {why}", self.artifact, self.path)
            }
            PinStatus::Drift(drift) => {
                let mut text = format!(
                    "{} ({}): the schema changed since it was exported\n",
                    self.artifact, self.path
                );
                for change in &drift.changes {
                    text.push_str("    ");
                    text.push_str(change);
                    text.push('\n');
                }
                if drift.more > 0 {
                    text.push_str(&format!("    … and {} more\n", drift.more));
                }
                text.push_str("  Run `graphpg schema export` to update the file.");
                text
            }
        }
    }
}

/// Compares `artifacts` with the files at the paths of `config.export`.
pub fn check(artifacts: &Artifacts, config: &Config) -> Vec<PinResult> {
    let mut results = vec![PinResult {
        artifact: "graphql",
        path: config.export.graphql.clone(),
        status: check_text(&artifacts.graphql, &config.export.graphql),
    }];
    if let Some(subgraph) = &artifacts.subgraph {
        results.push(PinResult {
            artifact: "subgraph",
            path: config.export.subgraph.clone(),
            status: check_text(subgraph, &config.export.subgraph),
        });
    }
    results.push(PinResult {
        artifact: "openapi",
        path: config.export.openapi.clone(),
        status: check_openapi(&artifacts.openapi, &config.export.openapi),
    });
    results
}

/// What a startup check found, by what the config does about it.
pub fn drifted(results: &[PinResult]) -> Vec<&PinResult> {
    results
        .iter()
        .filter(|r| matches!(r.status, PinStatus::Drift(_) | PinStatus::Unreadable(_)))
        .collect()
}

/// The checks a startup runs: none with `pin: off`.
pub fn startup_check(artifacts: &Artifacts, config: &Config) -> Vec<PinResult> {
    if config.export.pin == PinMode::Off {
        return Vec::new();
    }
    check(artifacts, config)
}

fn normalize(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn check_text(generated: &str, path: &str) -> PinStatus {
    let pinned = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PinStatus::Missing,
        Err(e) => return PinStatus::Unreadable(e.to_string()),
    };
    let (pinned, generated) = (normalize(&pinned), normalize(generated));
    if pinned == generated {
        return PinStatus::Match;
    }
    let diff = TextDiff::from_lines(&pinned, &generated);
    let mut changes = Vec::new();
    let mut more = 0;
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => continue,
        };
        if changes.len() < SUMMARY_LINES {
            changes.push(format!("{sign} {}", change.value().trim_end()));
        } else {
            more += 1;
        }
    }
    PinStatus::Drift(Drift { changes, more })
}

fn check_openapi(generated: &Value, path: &str) -> PinStatus {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PinStatus::Missing,
        Err(e) => return PinStatus::Unreadable(e.to_string()),
    };
    // The file as `schema export` writes it: no need to parse it (a large schema's document
    // is tens of megabytes, and parsed, several times that)
    if normalize(&text) == normalize(&render_openapi(generated, path)) {
        return PinStatus::Match;
    }
    let pinned: Result<Value, String> = if is_yaml(path) {
        serde_yaml::from_str(&text).map_err(|e| e.to_string())
    } else {
        serde_json::from_str(&text).map_err(|e| e.to_string())
    };
    let pinned = match pinned {
        Ok(value) => value,
        Err(e) => return PinStatus::Unreadable(e),
    };
    // Compared as documents: formatting and key order are not drift
    if &pinned == generated {
        return PinStatus::Match;
    }
    let mut changes = Vec::new();
    json_changes(&pinned, generated, "", &mut changes);
    let more = changes.len().saturating_sub(SUMMARY_LINES);
    changes.truncate(SUMMARY_LINES);
    PinStatus::Drift(Drift { changes, more })
}

/// The differences between two documents, by JSON pointer.
fn json_changes(pinned: &Value, generated: &Value, pointer: &str, out: &mut Vec<String>) {
    match (pinned, generated) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                let at = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                match b.get(key) {
                    None => out.push(format!("- {at}")),
                    Some(other) => json_changes(value, other, &at, out),
                }
            }
            for key in b.keys().filter(|k| !a.contains_key(*k)) {
                out.push(format!(
                    "+ {pointer}/{}",
                    key.replace('~', "~0").replace('/', "~1")
                ));
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                json_changes(x, y, &format!("{pointer}/{i}"), out);
            }
        }
        (a, b) if a == b => {}
        (a, b) => {
            let short = |v: &Value| {
                let text = v.to_string();
                if text.len() > 60 {
                    format!(
                        "{}…",
                        &text[..text.char_indices().nth(60).map_or(text.len(), |(i, _)| i)]
                    )
                } else {
                    text
                }
            };
            out.push(format!("~ {pointer}: {} → {}", short(a), short(b)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn documents_are_compared_by_content() {
        let a = json!({ "paths": { "/a": { "get": {} }, "/b": {} }, "info": { "version": "1" } });
        let b = json!({ "info": { "version": "2" }, "paths": { "/a": { "get": {} }, "/c": {} } });
        let mut out = Vec::new();
        json_changes(&a, &b, "", &mut out);
        out.sort();
        assert_eq!(
            out,
            [
                "+ /paths/~1c",
                "- /paths/~1b",
                "~ /info/version: \"1\" → \"2\""
            ]
        );
        let mut same = Vec::new();
        json_changes(&a, &a.clone(), "", &mut same);
        assert!(same.is_empty());
    }

    #[test]
    fn missing_matching_and_drifted_files() {
        let dir = std::env::temp_dir().join(format!("graphpg-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sdl = dir.join("schema.graphql");
        let sdl = sdl.to_str().unwrap();
        assert_eq!(check_text("type Query { a: Int }", sdl), PinStatus::Missing);

        std::fs::write(sdl, "type Query { a: Int }  \n\n").unwrap();
        assert_eq!(
            check_text("type Query { a: Int }\n", sdl),
            PinStatus::Match,
            "whitespace is not drift"
        );

        let PinStatus::Drift(drift) = check_text("type Query {\n  a: Int\n  b: Int\n}", sdl) else {
            panic!("drift");
        };
        assert!(
            drift
                .changes
                .iter()
                .any(|c| c.starts_with("- type Query { a: Int }")),
            "{:?}",
            drift.changes
        );
        assert!(
            drift.changes.iter().any(|c| c == "+   b: Int"),
            "{:?}",
            drift.changes
        );

        let json = dir.join("openapi.json");
        let json = json.to_str().unwrap();
        let doc = json!({ "openapi": "3.1.0", "paths": {} });
        std::fs::write(json, render_openapi(&doc, json)).unwrap();
        assert_eq!(check_openapi(&doc, json), PinStatus::Match);
        let changed = json!({ "openapi": "3.1.0", "paths": { "/x": {} } });
        let PinStatus::Drift(d) = check_openapi(&changed, json) else {
            panic!("drift");
        };
        assert_eq!(d.changes, ["+ /paths/~1x"]);

        let yaml = dir.join("openapi.yaml");
        let yaml = yaml.to_str().unwrap();
        std::fs::write(yaml, render_openapi(&doc, yaml)).unwrap();
        assert!(std::fs::read_to_string(yaml)
            .unwrap()
            .contains("openapi: 3.1.0"));
        assert_eq!(check_openapi(&doc, yaml), PinStatus::Match);
        std::fs::write(json, "{ not json").unwrap();
        assert!(matches!(
            check_openapi(&doc, json),
            PinStatus::Unreadable(_)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn long_drifts_are_summarised() {
        let a = json!({ "paths": (0..40).map(|i| (format!("/p{i}"), json!({}))).collect::<serde_json::Map<_, _>>() });
        let b = json!({ "paths": {} });
        let dir = std::env::temp_dir().join(format!("graphpg-pin-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("openapi.json");
        let path = path.to_str().unwrap();
        std::fs::write(path, render_openapi(&a, path)).unwrap();
        let PinStatus::Drift(d) = check_openapi(&b, path) else {
            panic!("drift");
        };
        assert_eq!(d.changes.len(), SUMMARY_LINES);
        assert_eq!(d.more, 15);
        let report = PinResult {
            artifact: "openapi",
            path: path.to_string(),
            status: PinStatus::Drift(d),
        }
        .report();
        assert!(
            report.contains("… and 15 more") && report.contains("graphpg schema export"),
            "{report}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
