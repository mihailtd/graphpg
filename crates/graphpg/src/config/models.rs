//! `graphpg.yaml`: the configuration of one graphpg server. Shared sections (`server`,
//! `database`, `auth`, `rbac`, `debug`) apply to every API; `graphql` and `openapi` hold what
//! belongs to one API, each switchable with `enabled`. Everything else describes the
//! generated APIs where the database cannot say what is wanted: rename, describe or hide
//! types and fields, choose which operations exist, declare the keys and relations of views,
//! and mount functions as fields. Those entries are keyed by database object name (stable
//! across renames) and shared by both APIs. Schema design itself (tables, constraints, types,
//! functions) belongs in SQL.
//!
//! One file serves every environment: `dev:` and `prod:` blocks hold the settings that differ,
//! merged over the rest for the mode the server starts in (see [`super::loader`]).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP listener.
    #[serde(default)]
    pub server: ServerConfig,
    /// The PostgreSQL connection, the schemas to expose and the read replicas.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Authentication (OAuth 2.0 bearer tokens) and the Row-Level Security session context.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Role-based access control (RBAC) schema pruning configuration.
    #[serde(default)]
    pub rbac: RbacConfig,
    /// Diagnostics added to responses. For development only.
    #[serde(default)]
    pub debug: DebugConfig,
    /// The GraphQL API.
    #[serde(default)]
    pub graphql: GraphqlConfig,
    /// The REST API and its OpenAPI description.
    #[serde(default)]
    pub openapi: OpenapiConfig,
    /// Tables, views, materialized views and foreign tables, by relation name.
    #[serde(default)]
    pub entities: BTreeMap<String, EntityConfig>,
    /// Table-valued functions and computed-column functions, by function name.
    #[serde(default)]
    pub functions: BTreeMap<String, FunctionConfig>,
    /// Enum and composite types: PostgreSQL types by type name, `CHECK`-closed text columns
    /// (exposed as enums) by `<table>.<column>`.
    #[serde(default)]
    pub types: BTreeMap<String, TypeConfig>,
    /// Objects created by PostgreSQL extensions.
    #[serde(default)]
    pub extensions: ExtensionsConfig,
    /// Search root fields (`usersFuzzySearch`, ...).
    #[serde(default)]
    pub search: SearchConfig,
}

/// A number given as text, which is what `${NAME}` produces: `port: '${PORT}'`.
fn lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + std::str::FromStr,
    T::Err: std::fmt::Display,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native<T> {
        Value(T),
        Text(String),
    }
    match Native::<T>::deserialize(deserializer)? {
        Native::Value(value) => Ok(value),
        Native::Text(text) => text
            .trim()
            .to_ascii_lowercase()
            .parse()
            .map_err(|e| serde::de::Error::custom(format!("invalid value {text:?}: {e}"))),
    }
}

/// A switch given as text, which is what `${NAME}` produces: `true`, `false`, `1`, `0`,
/// `yes`, `no`, `on` or `off`.
fn switch<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Native {
        Value(bool),
        Text(String),
    }
    match Native::deserialize(deserializer)? {
        Native::Value(value) => Ok(value),
        Native::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => Err(serde::de::Error::custom(format!(
                "invalid value {text:?} (true or false)"
            ))),
        },
    }
}

/// The HTTP listener.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address to listen on.
    #[serde(default = "default_host")]
    pub host: String,
    /// Port to listen on.
    #[serde(deserialize_with = "lenient", default = "default_port")]
    pub port: u16,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    4000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

/// Database connection used when the config names none.
pub const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@localhost:5432/graphpg";

/// Schema inspected when the config names none.
pub const DEFAULT_SCHEMA: &str = "app_public";

/// The PostgreSQL connection, the schemas to expose and the read replicas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Connection string. Keep credentials out of the file: `url: ${DATABASE_URL}`.
    #[serde(default = "default_database_url")]
    pub url: String,
    /// Connections in the pool.
    #[serde(deserialize_with = "lenient", default = "default_pool_size")]
    pub pool_size: usize,
    /// Schemas to expose, the first being the default schema (`app_public` when empty).
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Connection strings of read replicas: queries run on those that are standbys.
    #[serde(default)]
    pub replicas: Vec<String>,
    /// Append a sqlcommenter comment (route, field, trace context) to every statement.
    #[serde(deserialize_with = "switch", default = "default_true")]
    pub sql_comments: bool,
}

fn default_database_url() -> String {
    DEFAULT_DATABASE_URL.to_string()
}
fn default_pool_size() -> usize {
    10
}
fn default_true() -> bool {
    true
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_database_url(),
            pool_size: default_pool_size(),
            schemas: Vec::new(),
            replicas: Vec::new(),
            sql_comments: true,
        }
    }
}

impl DatabaseConfig {
    /// The schemas to inspect, the first being the default: [`schemas`](Self::schemas), else
    /// [`DEFAULT_SCHEMA`].
    pub fn resolved_schemas(&self) -> Vec<String> {
        if self.schemas.is_empty() {
            vec![DEFAULT_SCHEMA.to_string()]
        } else {
            self.schemas.clone()
        }
    }
}

/// Diagnostics added to responses. Never enable them in production.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DebugConfig {
    /// Include the generated SQL in the response `extensions`.
    #[serde(default, deserialize_with = "switch")]
    pub sql: bool,
    /// Include PostgreSQL's `EXPLAIN (ANALYZE)` output of every statement.
    #[serde(default, deserialize_with = "switch")]
    pub explain: bool,
}

/// The GraphQL API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphqlConfig {
    /// Serve the GraphQL API at `/graphql`.
    #[serde(deserialize_with = "switch", default = "default_true")]
    pub enabled: bool,
    /// Serve the GraphiQL IDE at `/graphiql`. On in `dev` mode, off in `prod`.
    #[serde(default, deserialize_with = "switch")]
    pub ui: bool,
    /// Apollo Federation configuration.
    #[serde(default)]
    pub federation: FederationConfig,
}

impl Default for GraphqlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ui: false,
            federation: FederationConfig::default(),
        }
    }
}

/// The REST API and its OpenAPI description.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenapiConfig {
    /// Serve the REST API. Off until the REST surface exists.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
}

/// Apollo Federation configuration for exposing this engine as a federated subgraph.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FederationConfig {
    /// Enable Apollo Federation subgraph mode.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Federation spec version (defaults to "v2.3").
    #[serde(default)]
    pub version: Option<String>,
}

/// Role-based access control (RBAC) schema pruning configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RbacConfig {
    /// Whether to prune the GraphQL schema based on PostgreSQL role privileges.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// Optional PostgreSQL role to switch to (`SET ROLE`) during schema inspection.
    /// If None, privileges are checked against the currently connected database user/role.
    #[serde(default)]
    pub role: Option<String>,
}

/// Authentication: the engine is an OAuth 2.0 resource server. Requests carry access tokens
/// as `Authorization: Bearer <token>` (RFC 6750); tokens are JWTs (RFC 9068) signed by the
/// authorization server, whose claims become the PostgreSQL role and the session settings
/// Row-Level Security policies read (`current_setting('request.jwt.claims', true)`).
///
/// Off by default: every request then runs as the engine's database user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Whether requests are authenticated.
    #[serde(default, deserialize_with = "switch")]
    pub enabled: bool,
    /// The authorization server's issuer identifier: a token's `iss` must equal it, and the
    /// signing keys are discovered from its metadata unless `jwks_uri` names them.
    #[serde(default)]
    pub issuer: Option<String>,
    /// This API's identifiers: a token's `aud` must contain one of them.
    #[serde(default, deserialize_with = "one_or_many")]
    pub audience: Vec<String>,
    /// URL of the authorization server's JSON Web Key Set, when it is not discovered from
    /// `issuer` (OpenID Connect discovery, then RFC 8414 metadata).
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// A shared secret verifying HMAC-signed tokens (HS256, HS384, HS512) instead of a key
    /// set, for authorization servers that sign that way. Keep it out of the file:
    /// `secret: ${AUTH_JWT_SECRET}`. At least 32 bytes.
    #[serde(default)]
    pub secret: Option<String>,
    /// Signature algorithms (`alg`) accepted. By default the asymmetric ones with a key set
    /// (RS256, RS384, RS512, PS256, PS384, PS512, ES256, ES384, EdDSA), HS256 with a secret.
    #[serde(default)]
    pub algorithms: Vec<String>,
    /// The `typ` header tokens must have, e.g. `at+jwt` (RFC 9068); not checked when unset,
    /// since many authorization servers issue `JWT`.
    #[serde(default)]
    pub token_type: Option<String>,
    /// Clock skew tolerated when checking `exp` and `nbf`, in seconds.
    #[serde(default = "default_leeway", deserialize_with = "lenient")]
    pub leeway_seconds: u64,
    /// The PostgreSQL roles requests run as.
    #[serde(default)]
    pub roles: RolesConfig,
    /// The session settings a request's claims are written to.
    #[serde(default)]
    pub session_variables: SessionVariablesConfig,
}

fn default_leeway() -> u64 {
    30
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer: None,
            audience: Vec::new(),
            jwks_uri: None,
            secret: None,
            algorithms: Vec::new(),
            token_type: None,
            leeway_seconds: default_leeway(),
            roles: RolesConfig::default(),
            session_variables: SessionVariablesConfig::default(),
        }
    }
}

/// A string or a list of strings.
fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// The PostgreSQL roles requests run as (`SET LOCAL ROLE`), fixed by deployment: tokens never
/// choose one. Their claims (an application `role` claim included) are data that Row-Level
/// Security policies compare with whatever the schema holds; what they mean is the
/// application's to decide. The engine's database user must be a member of each role, and
/// should itself hold no privileges (`NOINHERIT`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    /// Role of requests with a valid token. Unset, they run as the engine's database user,
    /// which must then neither own the tables nor bypass RLS for policies to apply.
    #[serde(default)]
    pub authenticated: Option<String>,
    /// Role of requests without a token. Unset, they are rejected (401).
    #[serde(default)]
    pub anonymous: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionVariablesConfig {
    /// Prefix of a setting per top-level claim (`request.jwt.claim.sub`); claims whose names
    /// are not identifiers get none. Empty: no per-claim settings.
    #[serde(default = "default_claim_prefix")]
    pub prefix: String,
    /// Setting holding every claim as JSON.
    #[serde(default = "default_claims_setting")]
    pub claims_setting: String,
    /// Further settings: claim (name or dot-separated path) -> setting name, e.g.
    /// `org_id: app.current_org_id`.
    #[serde(default)]
    pub mappings: BTreeMap<String, String>,
}

fn default_claim_prefix() -> String {
    "request.jwt.claim.".to_string()
}
fn default_claims_setting() -> String {
    "request.jwt.claims".to_string()
}

impl Default for SessionVariablesConfig {
    fn default() -> Self {
        Self {
            prefix: default_claim_prefix(),
            claims_setting: default_claims_setting(),
            mappings: BTreeMap::new(),
        }
    }
}

/// Search fields return the best matches, without pagination: `first` hits, `default_limit`
/// when it is left out, never more than `max_limit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    /// Hits returned without `first` (10).
    pub default_limit: Option<usize>,
    /// Largest accepted `first` (50).
    pub max_limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeConfig {
    /// GraphQL type name (a composite's input type is `<name>Input`).
    pub type_name: Option<String>,
    /// Type description (defaults to the type's SQL comment).
    pub description: Option<String>,
    /// Enums: GraphQL value per database label, e.g. `in-progress: ACTIVE`.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Composites: per-attribute changes, by attribute name.
    #[serde(default)]
    pub attributes: BTreeMap<String, AttributeConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeConfig {
    pub field_name: Option<String>,
    pub description: Option<String>,
}

/// Tables, views and functions owned by extensions (`pgcrypto`, `citext`, `postgis`, ...) are
/// hidden unless their extension is listed here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionsConfig {
    /// Extensions whose objects are exposed; `"*"` exposes every extension's objects.
    #[serde(default)]
    pub expose: Vec<String>,
}

impl ExtensionsConfig {
    pub fn exposes_all(&self) -> bool {
        self.expose.iter().any(|e| e == "*")
    }
}

/// Settings given outside the config file (environment variables and CLI flags), applied over
/// the file at startup and again on every hot reload so a reload sees the same effective
/// config as startup. Transitional: the config file, with `${ENV}` interpolation, replaces
/// them.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    /// `DATABASE_SCHEMAS`: comma-separated schemas, taking precedence over the config file.
    pub database_schemas: Option<String>,
    /// `DATABASE_SCHEMA`: one schema (or a comma-separated list), used when neither
    /// `DATABASE_SCHEMAS` nor the config file's `database.schemas` names any.
    pub database_schema: Option<String>,
    /// `--federation` / `FEDERATION`.
    pub federation: bool,
    /// `GRAPHIQL` (`--graphiql`): turns the GraphiQL IDE on or off.
    pub graphiql: Option<bool>,
    /// `AUTH_ENABLED` (`--auth` / `--no-auth`): turns authentication on or off.
    pub auth_enabled: Option<bool>,
    /// `AUTH_ISSUER`.
    pub auth_issuer: Option<String>,
    /// `AUTH_AUDIENCE`: comma-separated.
    pub auth_audience: Option<String>,
    /// `AUTH_JWKS_URI`.
    pub auth_jwks_uri: Option<String>,
    /// `AUTH_JWT_SECRET`.
    pub auth_secret: Option<String>,
}

impl ConfigOverrides {
    /// Applies the overrides to `config`. Afterwards `config.database.schemas` is the
    /// resolved, non-empty list of schemas to inspect, the first being the default schema:
    /// `DATABASE_SCHEMAS`, else the config file's `database.schemas`, else `DATABASE_SCHEMA`,
    /// else [`DEFAULT_SCHEMA`].
    pub fn apply(&self, config: &mut Config) {
        fn split(list: Option<&str>) -> Vec<String> {
            list.unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        }
        let from_env = split(self.database_schemas.as_deref());
        if !from_env.is_empty() {
            config.database.schemas = from_env;
        } else if config.database.schemas.is_empty() {
            config.database.schemas = split(self.database_schema.as_deref());
        }
        config.database.schemas = config.database.resolved_schemas();
        if self.federation {
            config.graphql.federation.enabled = true;
        }
        if let Some(graphiql) = self.graphiql {
            config.graphql.ui = graphiql;
        }
        // Empty variables (a template's `AUTH_JWT_SECRET=`) override nothing
        let set = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        if let Some(enabled) = self.auth_enabled {
            config.auth.enabled = enabled;
        }
        if let Some(issuer) = set(&self.auth_issuer) {
            config.auth.issuer = Some(issuer);
        }
        let audience = split(self.auth_audience.as_deref());
        if !audience.is_empty() {
            config.auth.audience = audience;
        }
        if let Some(uri) = set(&self.auth_jwks_uri) {
            config.auth.jwks_uri = Some(uri);
        }
        if let Some(secret) = set(&self.auth_secret) {
            config.auth.secret = Some(secret);
        }
    }
}

/// Root fields an entity can generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// Single row by primary key (`user(id: ...)`).
    Read,
    /// Plain list (`users`).
    List,
    /// Relay connection (`usersConnection`).
    Connection,
    /// Insert rows: also omits `create_many`.
    Create,
    /// Update rows (`updateUser`, `updateUserByEmail`): also omits `update_many` and `upsert`.
    Update,
    /// Delete rows (`deleteUser`, `deleteUserByEmail`): also omits `delete_many`.
    Delete,
    /// Insert several rows (`createUsers`).
    CreateMany,
    /// Update the rows matching a filter (`updateUsers`).
    UpdateMany,
    /// Delete the rows matching a filter (`deleteUsers`).
    DeleteMany,
    /// The `onConflict` argument of creates.
    Upsert,
}

impl Operation {
    pub const ALL: [Operation; 10] = [
        Self::Read,
        Self::List,
        Self::Connection,
        Self::Create,
        Self::Update,
        Self::Delete,
        Self::CreateMany,
        Self::UpdateMany,
        Self::DeleteMany,
        Self::Upsert,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::List => "list",
            Self::Connection => "connection",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::CreateMany => "create_many",
            Self::UpdateMany => "update_many",
            Self::DeleteMany => "delete_many",
            Self::Upsert => "upsert",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityConfig {
    /// GraphQL type name. Derived names follow it (`<Type>Filter`, `create<Type>`, ...).
    pub type_name: Option<String>,
    /// Type description (defaults to the relation's SQL comment).
    pub description: Option<String>,
    /// Deprecate the type's root fields and the relation fields leading to it, with this
    /// reason (also `@deprecated <reason>` in the SQL comment).
    pub deprecated: Option<String>,
    /// Remove the type, its root fields and every relation field pointing at it.
    #[serde(default)]
    pub omit: bool,
    /// Root fields not to generate.
    #[serde(default)]
    pub omit_operations: Vec<Operation>,
    /// Name of the list root field.
    pub list_field: Option<String>,
    /// Name of the single-row-by-key root field.
    pub single_field: Option<String>,
    /// Name of the connection root field.
    pub connection_field: Option<String>,
    /// Key columns of a relation without a primary key (views, materialized views). Gives
    /// the type row lookups, stable cursors and, for a single column, a Relay `id`.
    #[serde(default)]
    pub primary_key: Vec<String>,
    /// Column a trigger maintains as the row's version: `rowVersion` reads it and
    /// `expectedVersion` is compared with it. Without one, rows are versioned by `xmin`,
    /// which every table has (views need a version column to be versioned at all).
    pub version_column: Option<String>,
    /// Per-column changes, by column name.
    #[serde(default)]
    pub columns: BTreeMap<String, ColumnConfig>,
    /// Relation fields of foreign keys, by constraint name. A name that is not a constraint
    /// of the relation declares a new (virtual) foreign key and needs `columns` and `references`.
    #[serde(default)]
    pub foreign_keys: BTreeMap<String, ForeignKeyConfig>,
    /// Apollo Federation key fields override (e.g. `"id"` or `"tenantId userId"`).
    #[serde(default)]
    pub key_fields: Option<String>,
    /// Mark entity as @shareable in Apollo Federation.
    #[serde(default)]
    pub shareable: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnConfig {
    /// GraphQL field name (also used in inputs, filters and `orderBy` values).
    pub field_name: Option<String>,
    /// Field description (defaults to the column's SQL comment).
    pub description: Option<String>,
    /// Deprecate the field, with this reason (also `@deprecated <reason>` in the SQL comment).
    pub deprecated: Option<String>,
    /// Hide the column from every type, input, filter and ordering.
    #[serde(default)]
    pub omit: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyConfig {
    /// Referencing columns (declares a virtual foreign key).
    #[serde(default)]
    pub columns: Vec<String>,
    /// Referenced relation and columns (declares a virtual foreign key).
    pub references: Option<ReferenceConfig>,
    /// Name of the to-one relation field on this type.
    pub field_name: Option<String>,
    /// Name of the to-many relation field on the referenced type; its connection field is
    /// `<name>Connection`.
    pub reverse_field_name: Option<String>,
    /// Hide both relation fields.
    #[serde(default)]
    pub omit: bool,
    /// Hide the to-many relation fields on the referenced type.
    #[serde(default)]
    pub omit_reverse: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceConfig {
    /// Referenced relation, in the same schema.
    pub table: String,
    /// Referenced columns (default: its primary key).
    #[serde(default)]
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionConfig {
    /// Root Query field name (table-valued functions) or field name on the table's type
    /// (computed columns).
    pub field_name: Option<String>,
    /// Field description (defaults to the function's SQL comment).
    pub description: Option<String>,
    /// Deprecate the function's fields, with this reason.
    pub deprecated: Option<String>,
    /// Remove the root Query field, or the computed column field.
    #[serde(default)]
    pub omit: bool,
    /// Expose a table-valued function as a field of a table/view type too.
    pub mount: Option<MountConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountConfig {
    /// Relation whose type gets the field (default: the one detected from the function's
    /// first argument).
    pub on: Option<String>,
    /// Field name on that type.
    pub field_name: Option<String>,
    /// Function arguments filled from the parent row: `argument: column`. Default: the first
    /// argument gets the parent's single-column primary key. Other arguments become field
    /// arguments.
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    /// Do not mount (also disables a detected mount).
    #[serde(default)]
    pub omit: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sample_yaml_config() {
        let yaml_str = r#"
entities:
  users:
    type_name: Person
    omit_operations: [delete]
    columns:
      password_hash: { omit: true }
      birth_date: { field_name: dateOfBirth }

  user_contact_view:
    primary_key: [user_id, value]
    foreign_keys:
      user_contact_view_user:
        columns: [user_id]
        references: { table: users }
        field_name: user
        reverse_field_name: contacts

functions:
  flatten_menu_items:
    mount:
      on: menus
      field_name: flattenedItems
      arguments: { m_id: id }
"#;
        let config = Config::from_yaml(yaml_str).expect("failed to parse YAML");
        assert_eq!(config.entities.len(), 2);

        let users = &config.entities["users"];
        assert_eq!(users.type_name.as_deref(), Some("Person"));
        assert_eq!(users.omit_operations, vec![Operation::Delete]);
        assert!(users.columns["password_hash"].omit);
        assert_eq!(
            users.columns["birth_date"].field_name.as_deref(),
            Some("dateOfBirth")
        );

        let view = &config.entities["user_contact_view"];
        assert_eq!(view.primary_key, vec!["user_id", "value"]);
        let fk = &view.foreign_keys["user_contact_view_user"];
        assert_eq!(fk.columns, vec!["user_id"]);
        assert_eq!(fk.references.as_ref().unwrap().table, "users");
        assert!(fk.references.as_ref().unwrap().columns.is_empty());
        assert_eq!(fk.reverse_field_name.as_deref(), Some("contacts"));

        let mount = config.functions["flatten_menu_items"]
            .mount
            .as_ref()
            .unwrap();
        assert_eq!(mount.on.as_deref(), Some("menus"));
        assert_eq!(mount.arguments["m_id"], "id");
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = Config::from_yaml("entities:\n  users:\n    rename_type: X\n").unwrap_err();
        assert!(err.to_string().contains("rename_type"), "{err}");
    }

    #[test]
    fn empty_config() {
        let config = Config::from_yaml("  \n").unwrap();
        assert!(config.entities.is_empty() && config.functions.is_empty());
    }
}
