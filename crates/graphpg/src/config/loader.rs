//! Loading `graphpg.yaml`: one file for every environment.
//!
//! The effective config of a [`Mode`] is built in four steps, each overriding the last:
//!
//! 1. the built-in defaults of every field,
//! 2. the built-in defaults of the mode (`dev` serves the GraphiQL IDE and the Scalar API
//!    reference, `prod` does not),
//! 3. the file's own settings,
//! 4. the file's `dev:` or `prod:` block, which holds the settings that differ between modes.
//!
//! Steps 2 to 4 are deep merges: mappings merge key by key, anything else (lists included)
//! replaces. String values may read the environment, `${NAME}` or `${NAME:-default}`, so that
//! credentials stay out of the file (quote them inside `[...]` and `{...}`); `$${` writes a
//! literal `${`. The merged document is
//! parsed strictly: an unknown key is an error, in the settings of the mode being started and
//! in those of the other one alike, so a typo in `dev:` is found when starting `prod`.
use super::models::Config;
use serde_yaml::{Mapping, Value};
use std::fmt;
use std::path::Path;

/// File read when `--config` names none and it exists.
pub const DEFAULT_CONFIG_FILE: &str = "graphpg.yaml";

/// How the server runs: the settings of one mode are applied over the file's shared ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Development: UIs on, the schema watched. Selected explicitly.
    Dev,
    /// Production, the default: secure unless a setting says otherwise.
    #[default]
    Prod,
}

impl Mode {
    /// The key of the mode's block in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Dev => "dev",
            Mode::Prod => "prod",
        }
    }

    const ALL: [Mode; 2] = [Mode::Dev, Mode::Prod];

    /// Settings that differ from the field defaults by mode.
    fn defaults(self) -> &'static str {
        match self {
            Mode::Dev => {
                "graphql:\n  ui: true\nopenapi:\n  ui: true\n  spec: true\nwatch:\n  enabled: true\n"
            }
            Mode::Prod => "{}",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" => Ok(Mode::Dev),
            "prod" | "production" => Ok(Mode::Prod),
            other => Err(format!("unknown mode {other:?} (dev or prod)")),
        }
    }
}

/// Resolves the mode from the `--mode` flag (if given), else the `GRAPHPG_MODE` environment variable
/// (if set and non-empty), else [`Mode::Prod`].
pub fn resolve_mode(flag: Option<&str>, env: Option<&str>) -> Result<Mode, String> {
    if let Some(m) = flag {
        return m.parse();
    }
    if let Some(m) = env {
        if !m.is_empty() {
            return m.parse();
        }
    }
    Ok(Mode::Prod)
}

/// What was wrong with a config file: every problem found, not only the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub problems: Vec<String>,
}

impl ConfigError {
    fn one(problem: impl Into<String>) -> Self {
        Self {
            problems: vec![problem.into()],
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.problems.join("\n"))
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Parses `yaml` for production, reading `${NAME}` from the process environment.
    pub fn from_yaml(yaml: &str) -> Result<Self, ConfigError> {
        Self::parse(yaml, Mode::Prod, &|name| std::env::var(name).ok())
    }

    /// The config of `mode` when there is no file.
    pub fn defaults(mode: Mode) -> Self {
        Self::parse("", mode, &|_| None).expect("the built-in defaults are valid")
    }

    /// Loads the file at `path` for `mode`, reading `${NAME}` from the process environment.
    pub fn load_from_file(path: impl AsRef<Path>, mode: Mode) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let yaml = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::one(format!("{}: {e}", path.display())))?;
        Self::parse(&yaml, mode, &|name| std::env::var(name).ok()).map_err(|e| ConfigError {
            problems: e
                .problems
                .into_iter()
                .map(|p| format!("{}: {p}", path.display()))
                .collect(),
        })
    }

    /// Parses `yaml` for `mode`; `env` answers `${NAME}`. An empty file is the default config.
    pub fn parse(
        yaml: &str,
        mode: Mode,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let document: Value = if yaml.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            serde_yaml::from_str(yaml).map_err(|e| ConfigError::one(e.to_string()))?
        };
        let Value::Mapping(mut shared) = document else {
            return Err(ConfigError::one("the config must be a mapping of settings"));
        };

        // `dev:` and `prod:` are not settings: they are taken out before the rest is parsed
        let mut blocks = Vec::new();
        for m in Mode::ALL {
            match shared.remove(m.as_str()) {
                None | Some(Value::Null) => blocks.push((m, Mapping::new())),
                Some(Value::Mapping(block)) => blocks.push((m, block)),
                Some(_) => {
                    return Err(ConfigError::one(format!(
                        "{m}: must be a mapping of the settings that differ in {m} mode"
                    )))
                }
            }
        }

        let mut problems = Vec::new();
        check_removed_secret_keys(&Value::Mapping(shared.clone()), "", &mut problems);
        for (m, block) in &blocks {
            check_removed_secret_keys(&Value::Mapping(block.clone()), m.as_str(), &mut problems);
        }
        if !problems.is_empty() {
            return Err(ConfigError { problems });
        }

        let mut shared = Value::Mapping(shared);
        normalize_rest_key(&mut shared);
        interpolate(&mut shared, "", env, &mut problems);
        let blocks: Vec<(Mode, Value)> = blocks
            .into_iter()
            .map(|(m, block)| {
                let mut block = Value::Mapping(block);
                normalize_rest_key(&mut block);
                interpolate(&mut block, m.as_str(), env, &mut problems);
                (m, block)
            })
            .collect();
        if !problems.is_empty() {
            return Err(ConfigError { problems });
        }

        // The requested mode first, so its error is the one reported when the shared
        // settings are at fault
        let mut requested = None;
        for m in std::iter::once(mode).chain(Mode::ALL.into_iter().filter(|m| *m != mode)) {
            let block = &blocks
                .iter()
                .find(|(bm, _)| *bm == m)
                .expect("both modes")
                .1;
            let mut merged: Value =
                serde_yaml::from_str(m.defaults()).expect("built-in mode defaults parse");
            merge(&mut merged, shared.clone());
            merge(&mut merged, block.clone());
            let config = match serde_yaml::from_value::<Config>(merged) {
                Ok(config) => config,
                Err(e) if m == mode => return Err(ConfigError::one(e.to_string())),
                Err(e) => return Err(ConfigError::one(format!("{m}: {e}"))),
            };
            if m == Mode::Prod {
                if config.debug.sql {
                    return Err(ConfigError::one(
                        "debug.sql is only allowed in the dev: block",
                    ));
                }
                if config.debug.explain {
                    return Err(ConfigError::one(
                        "debug.explain is only allowed in the dev: block",
                    ));
                }
            }
            if m == mode {
                requested = Some(config);
            }
        }
        Ok(requested.expect("the requested mode was built"))
    }
}

fn check_removed_secret_keys(val: &Value, prefix: &str, problems: &mut Vec<String>) {
    if let Value::Mapping(map) = val {
        for (k, v) in map {
            let key_str = match k.as_str() {
                Some(s) => s,
                None => continue,
            };
            let path = if prefix.is_empty() {
                key_str.to_string()
            } else {
                format!("{prefix}.{key_str}")
            };
            if path == "database.url" || path.ends_with(".database.url") {
                problems.push(format!(
                    "{path} is not a config key: the connection string is a secret, set DATABASE_URL"
                ));
            } else if path == "database.replicas" || path.ends_with(".database.replicas") {
                problems.push(format!(
                    "{path} is not a config key: replica URLs are secrets, set DATABASE_REPLICA_URLS"
                ));
            } else if path == "auth.secret" || path.ends_with(".auth.secret") {
                problems.push(format!(
                    "{path} is not a config key: the JWT secret is a secret, set AUTH_JWT_SECRET"
                ));
            }
            check_removed_secret_keys(v, &path, problems);
        }
    }
}

fn normalize_rest_key(val: &mut Value) {
    match val {
        Value::Mapping(map) => {
            let rest_key = Value::String("rest".to_string());
            let openapi_key = Value::String("openapi".to_string());
            if let Some(rest_val) = map.remove(&rest_key) {
                match map.get_mut(&openapi_key) {
                    Some(openapi_val) => merge(openapi_val, rest_val),
                    None => {
                        map.insert(openapi_key.clone(), rest_val);
                    }
                }
            }
            if let Some(Value::Mapping(openapi_map)) = map.get_mut(&openapi_key) {
                let schema_key = Value::String("schema".to_string());
                if let Some(schema_val) = openapi_map.remove(&schema_key) {
                    openapi_map.insert(Value::String("spec".to_string()), schema_val);
                }
                let spec_path_key = Value::String("spec_path".to_string());
                if let Some(path_val) = openapi_map.remove(&spec_path_key) {
                    openapi_map.insert(Value::String("path".to_string()), path_val);
                }
            }
            for (_, v) in map.iter_mut() {
                normalize_rest_key(v);
            }
        }
        Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                normalize_rest_key(v);
            }
        }
        _ => {}
    }
}

/// Deep merge: mappings merge key by key, anything else in `over` replaces `base`.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Mapping(base), Value::Mapping(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

/// Replaces `${NAME}` and `${NAME:-default}` in every string below `value`, noting each one
/// that cannot be resolved at `path`.
fn interpolate(
    value: &mut Value,
    path: &str,
    env: &dyn Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) {
    match value {
        Value::String(text) if text.contains('$') => match expand(text, env) {
            Ok(expanded) => *text = expanded,
            Err(errors) => problems.extend(errors.into_iter().map(|e| {
                if path.is_empty() {
                    e
                } else {
                    format!("{path}: {e}")
                }
            })),
        },
        Value::Sequence(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                interpolate(item, &format!("{path}[{i}]"), env, problems);
            }
        }
        Value::Mapping(map) => {
            for (key, item) in map.iter_mut() {
                let name = key
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{key:?}"));
                let child = if path.is_empty() {
                    name
                } else {
                    format!("{path}.{name}")
                };
                interpolate(item, &child, env, problems);
            }
        }
        _ => {}
    }
}

/// Expands the references in one string.
fn expand(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, Vec<String>> {
    let mut out = String::with_capacity(text.len());
    let mut errors = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("$$") {
            out.push('$');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                errors.push(format!("`{rest}`: unterminated `${{`"));
                return Err(errors);
            };
            let reference = &after[..end];
            let (name, default) = match reference.split_once(":-") {
                Some((name, default)) => (name, Some(default)),
                None => (reference, None),
            };
            if !is_variable_name(name) {
                errors.push(format!("`${{{reference}}}`: not a variable name"));
            } else {
                match (env(name).filter(|v| !v.is_empty()), default) {
                    (Some(value), _) => out.push_str(&value),
                    (None, Some(default)) => out.push_str(default),
                    (None, None) => errors.push(format!(
                        "the environment variable `{name}` is not set (write `${{{name}:-default}}` for a default)"
                    )),
                }
            }
            rest = &after[end + 1..];
        } else {
            // A lone `$` is text
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: HashMap<&str, &str> = vars.iter().copied().collect();
        move |name| map.get(name).map(|v| v.to_string())
    }

    fn parse(yaml: &str, mode: Mode) -> Result<Config, ConfigError> {
        Config::parse(yaml, mode, &env_of(&[]))
    }

    #[test]
    fn empty_file_is_the_default_config() {
        let config = parse("  \n", Mode::Prod).unwrap();
        assert_eq!(config.server.port, 4000);
        assert_eq!(config.database.pool_size, 10);
        assert!(config.graphql.enabled);
        assert!(config.openapi.enabled);
        assert!(!config.graphql.ui && !config.openapi.ui);
        assert!(!config.watch.enabled && config.watch.export);
        assert_eq!(config.export.graphql, "schema.graphql");
        assert_eq!(config.export.subgraph, "subgraph.graphql");
        assert_eq!(config.export.config_schema, "graphpg.schema.json");
        assert_eq!(config.database.resolved_schemas(), vec!["app_public"]);
    }

    #[test]
    fn modes_differ_by_their_defaults_and_blocks() {
        let yaml = "
database: { pool_size: 20 }
graphql: { federation: { enabled: true } }
dev:
  database: { pool_size: 2 }
prod:
  graphql: { ui: false }
";
        let dev = parse(yaml, Mode::Dev).unwrap();
        let prod = parse(yaml, Mode::Prod).unwrap();
        // The mode's defaults, then the shared settings, then the mode's block
        assert!(dev.graphql.ui && !prod.graphql.ui);
        // Both UIs follow the mode
        assert!(dev.openapi.ui && !prod.openapi.ui);
        // Watch setting follows the mode
        assert!(dev.watch.enabled && !prod.watch.enabled);
        assert!(dev.watch.export && prod.watch.export);
        assert_eq!((dev.database.pool_size, prod.database.pool_size), (2, 20));
        // Deep merge: a block's setting does not drop its siblings
        assert!(dev.graphql.federation.enabled && prod.graphql.federation.enabled);
    }

    #[test]
    fn test_openapi_config() {
        // default Prod
        let cfg = parse("", Mode::Prod).unwrap();
        assert!(cfg.graphql.enabled);
        assert!(!cfg.graphql.ui);
        assert!(cfg.openapi.enabled);
        assert!(!cfg.openapi.ui);
        assert!(!cfg.openapi.spec);
        assert_eq!(cfg.openapi.path, "/openapi.json");

        // default Dev
        let cfg = parse("", Mode::Dev).unwrap();
        assert!(cfg.graphql.enabled);
        assert!(cfg.graphql.ui);
        assert!(cfg.openapi.enabled);
        assert!(cfg.openapi.ui);
        assert!(cfg.openapi.spec);
        assert_eq!(cfg.openapi.path, "/openapi.json");

        // using openapi key
        let yaml = "openapi: { enabled: false, ui: true, spec: true, path: '/api-docs.json' }";
        let cfg = parse(yaml, Mode::Prod).unwrap();
        assert!(!cfg.openapi.enabled);
        assert!(cfg.openapi.ui);
        assert!(cfg.openapi.spec);
        assert_eq!(cfg.openapi.path, "/api-docs.json");

        // using rest alias key and schema alias
        let yaml = "rest: { enabled: false, ui: true, schema: true, spec_path: '/spec.json' }";
        let cfg = parse(yaml, Mode::Prod).unwrap();
        assert!(!cfg.openapi.enabled);
        assert!(cfg.openapi.ui);
        assert!(cfg.openapi.spec);
        assert_eq!(cfg.openapi.path, "/spec.json");

        // using both merges with openapi taking precedence
        let yaml = "rest: { enabled: false }\nopenapi: { ui: true, spec: false }";
        let cfg = parse(yaml, Mode::Dev).unwrap();
        assert!(!cfg.openapi.enabled);
        assert!(cfg.openapi.ui);
        assert!(!cfg.openapi.spec);
    }

    #[test]
    fn a_block_overrides_a_mode_default() {
        let config = parse(
            "dev: { graphql: { ui: false }, watch: { enabled: false } }",
            Mode::Dev,
        )
        .unwrap();
        assert!(!config.graphql.ui);
        assert!(!config.watch.enabled);
    }

    #[test]
    fn lists_are_replaced_not_merged() {
        let yaml = "database: { schemas: [a, b] }\ndev: { database: { schemas: [c] } }";
        assert_eq!(parse(yaml, Mode::Dev).unwrap().database.schemas, ["c"]);
        assert_eq!(
            parse(yaml, Mode::Prod).unwrap().database.schemas,
            ["a", "b"]
        );
    }

    #[test]
    fn unknown_keys_are_errors_in_both_modes() {
        let err = parse("graphql: { uii: true }", Mode::Prod).unwrap_err();
        assert!(err.to_string().contains("unknown field `uii`"), "{err}");
        // A typo in the other mode's block is found too
        let err = parse("dev: { server: { prot: 1 } }", Mode::Prod).unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("dev: ") && text.contains("unknown field `prot`"),
            "{text}"
        );
        // `dev` and `prod` are blocks, not settings
        assert!(parse("dev: 3", Mode::Prod).is_err());
    }

    #[test]
    fn removed_secret_keys_produce_helpful_errors() {
        let err = parse("database: { url: 'postgres://localhost/db' }", Mode::Prod).unwrap_err();
        assert_eq!(
            err.problems[0],
            "database.url is not a config key: the connection string is a secret, set DATABASE_URL"
        );

        let err = parse(
            "database: { replicas: ['postgres://localhost/replica'] }",
            Mode::Prod,
        )
        .unwrap_err();
        assert_eq!(
            err.problems[0],
            "database.replicas is not a config key: replica URLs are secrets, set DATABASE_REPLICA_URLS"
        );

        let err = parse(
            "auth: { secret: 'secret123456789012345678901234567890' }",
            Mode::Prod,
        )
        .unwrap_err();
        assert_eq!(
            err.problems[0],
            "auth.secret is not a config key: the JWT secret is a secret, set AUTH_JWT_SECRET"
        );

        let err = parse(
            "dev: { database: { url: 'postgres://localhost/db' } }",
            Mode::Prod,
        )
        .unwrap_err();
        assert_eq!(
            err.problems[0],
            "dev.database.url is not a config key: the connection string is a secret, set DATABASE_URL"
        );
    }

    #[test]
    fn debug_is_dev_only() {
        // debug.sql in shared section fails even when starting dev
        let err = parse("debug: { sql: true }", Mode::Dev).unwrap_err();
        assert_eq!(
            err.to_string(),
            "debug.sql is only allowed in the dev: block"
        );

        // debug.explain in shared section fails even when starting dev
        let err = parse("debug: { explain: true }", Mode::Dev).unwrap_err();
        assert_eq!(
            err.to_string(),
            "debug.explain is only allowed in the dev: block"
        );

        // in prod block it fails
        let err = parse("prod: { debug: { sql: true } }", Mode::Prod).unwrap_err();
        assert_eq!(
            err.to_string(),
            "debug.sql is only allowed in the dev: block"
        );

        // in dev block it is accepted for dev and prod
        let dev = parse("dev: { debug: { sql: true, explain: true } }", Mode::Dev).unwrap();
        assert!(dev.debug.sql && dev.debug.explain);
        let prod = parse("dev: { debug: { sql: true, explain: true } }", Mode::Prod).unwrap();
        assert!(!prod.debug.sql && !prod.debug.explain);
    }

    #[test]
    fn environment_references_are_expanded_in_strings() {
        let env = env_of(&[("HOST", "127.0.0.1"), ("SCHEMA", "billing")]);
        let yaml = "
server:
  host: ${HOST}
database:
  schemas: ['${SCHEMA}', '${OTHER:-public}']
";
        let config = Config::parse(yaml, Mode::Prod, &env).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.database.schemas, ["billing", "public"]);
    }

    #[test]
    fn missing_variables_are_all_reported_with_their_path() {
        let yaml = "
server: { host: '${A}' }
auth: { issuer: 'https://${B}' }
dev:
  server: { host: '${C}' }
";
        let err = parse(yaml, Mode::Prod).unwrap_err();
        assert_eq!(err.problems.len(), 3, "{err}");
        assert!(err.problems[0].starts_with("server.host: the environment variable `A`"));
        assert!(err.problems[1].starts_with("auth.issuer: the environment variable `B`"));
        assert!(err.problems[2].starts_with("dev.server.host: the environment variable `C`"));
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let env = env_of(&[("EMPTY", "")]);
        let config =
            Config::parse("server: { host: '${EMPTY:-fallback}' }", Mode::Prod, &env).unwrap();
        assert_eq!(config.server.host, "fallback");
        assert!(Config::parse("server: { host: '${EMPTY}' }", Mode::Prod, &env).is_err());
    }

    #[test]
    fn dollars_can_be_escaped_and_lone_ones_are_text() {
        let config = parse("server: { host: 'a$$ {b} $c $${D}' }", Mode::Prod).unwrap();
        assert_eq!(config.server.host, "a$ {b} $c ${D}");
        let err = parse("server: { host: '${A' }", Mode::Prod).unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
        let err = parse("server: { host: '${1A}' }", Mode::Prod).unwrap_err();
        assert!(err.to_string().contains("not a variable name"), "{err}");
    }

    #[test]
    fn numbers_and_switches_may_come_from_text() {
        let env = env_of(&[("PORT", "8080"), ("SQL_COMMENTS", "off"), ("POOL", "7")]);
        let yaml = "
server: { port: '${PORT}' }
database: { sql_comments: '${SQL_COMMENTS}', pool_size: '${POOL}' }
graphql: { ui: 'true' }
";
        let config = Config::parse(yaml, Mode::Prod, &env).unwrap();
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.database.pool_size, 7);
        assert!(!config.database.sql_comments);
        assert!(config.graphql.ui);
        let err = parse("server: { port: 'many' }", Mode::Prod).unwrap_err();
        assert!(err.to_string().contains("many"), "{err}");
    }

    #[test]
    fn modes_parse_from_text() {
        assert_eq!("dev".parse::<Mode>(), Ok(Mode::Dev));
        assert_eq!("Production".parse::<Mode>(), Ok(Mode::Prod));
        assert!("staging".parse::<Mode>().is_err());
        assert_eq!(Mode::default(), Mode::Prod);
    }

    #[test]
    fn test_resolve_mode() {
        // default is prod
        assert_eq!(resolve_mode(None, None).unwrap(), Mode::Prod);
        assert_eq!(resolve_mode(None, Some("")).unwrap(), Mode::Prod);

        // env beats default
        assert_eq!(resolve_mode(None, Some("dev")).unwrap(), Mode::Dev);
        assert_eq!(resolve_mode(None, Some("development")).unwrap(), Mode::Dev);
        assert_eq!(resolve_mode(None, Some("prod")).unwrap(), Mode::Prod);

        // flag beats env
        assert_eq!(resolve_mode(Some("dev"), Some("prod")).unwrap(), Mode::Dev);
        assert_eq!(resolve_mode(Some("prod"), Some("dev")).unwrap(), Mode::Prod);

        // invalid values error
        assert!(resolve_mode(Some("invalid"), None).is_err());
        assert!(resolve_mode(None, Some("invalid")).is_err());
        assert!(resolve_mode(Some("invalid"), Some("dev")).is_err());
    }
}
