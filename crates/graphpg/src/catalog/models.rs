use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Custom GraphQL scalars, with the wire format of their values.
pub const CUSTOM_SCALARS: &[(&str, &str)] = &[
    ("JSON", "Any JSON value."),
    ("BigInt", "A 64-bit integer, as a string (JSON numbers are only exact up to 2^53). Input also takes an integer number."),
    ("Decimal", "An arbitrary-precision decimal number, as a string, e.g. `12.50`. Input also takes a number."),
    ("Datetime", "A point in time in ISO 8601 format with a UTC offset, e.g. `2024-05-01T12:30:00+00:00`. Input without an offset is in the server's time zone."),
    ("LocalDatetime", "A date and time of day without a time zone, in ISO 8601 format, e.g. `2024-05-01T12:30:00`."),
    ("Date", "A calendar date in ISO 8601 format, e.g. `2024-05-01`."),
    ("Time", "A time of day in ISO 8601 format, e.g. `12:30:00`."),
    ("TimeTz", "A time of day with a UTC offset, e.g. `12:30:00+02`."),
    ("Interval", "A duration in ISO 8601 format, e.g. `P1DT2H30M` (1 day, 2 hours and 30 minutes)."),
    ("UUID", "A universally unique identifier, e.g. `a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11`."),
    ("Base64", "Binary data, base64-encoded (RFC 4648, with padding)."),
    ("InternetAddress", "An IPv4 or IPv6 host address, optionally with a network prefix length, e.g. `192.168.0.1/24`."),
    ("CidrAddress", "An IPv4 or IPv6 network, e.g. `192.168.0.0/24` (no bits set after the prefix)."),
    ("MacAddress", "A MAC address of 6 (or, for `macaddr8`, 8) bytes, e.g. `08:00:2b:01:02:03`."),
    ("GeoJSON", "A geometry as a GeoJSON geometry object (RFC 7946), e.g. `{\"type\": \"Point\", \"coordinates\": [2.35, 48.85]}`: longitude, then latitude, in WGS 84 unless the column says otherwise."),
];

/// GraphQL scalars of range bounds (`IntRange` is a range of `Int`s).
pub const RANGE_SCALARS: &[&str] = &["Int", "BigInt", "Decimal", "Date", "Datetime"];

/// `interval` as an ISO 8601 duration (`{v}` is the value): `P1Y2M3DT4H5M6.5S`, zero parts
/// left out, `PT0S` for none. Parts may be negative, as PostgreSQL's `iso_8601` style does.
const INTERVAL_ISO_8601: &str = "CASE WHEN {v} IS NULL THEN NULL ELSE coalesce(nullif(concat('P', \
    nullif(extract(year FROM {v}), 0) || 'Y', nullif(extract(month FROM {v}), 0) || 'M', nullif(extract(day FROM {v}), 0) || 'D', \
    'T' || nullif(concat(nullif(extract(hour FROM {v}), 0) || 'H', nullif(extract(minute FROM {v}), 0) || 'M', \
    nullif(trim_scale(extract(second FROM {v})), 0) || 'S'), '')), 'P'), 'PT0S') END";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PgType {
    Uuid,
    Text,
    Varchar,
    Char,
    Int2,
    Int4,
    Int8,
    Bool,
    Float4,
    Float8,
    Numeric,
    Timestamp,
    TimestampTz,
    Date,
    Time,
    TimeTz,
    Interval,
    Json,
    Jsonb,
    Citext,
    Bytea,
    Money,
    Inet,
    Cidr,
    MacAddr,
    MacAddr8,
    /// `vector` (pgvector).
    Vector,
    /// `ltree` (a label path, e.g. `top.science.astronomy`).
    Ltree,
    /// `tsvector`: a document prepared for full-text search.
    TsVector,
    /// PostGIS `geometry` (planar) and `geography` (on the spheroid, distances in meters).
    Geometry,
    Geography,
    /// A multirange (`tstzmultirange`, ...): a set of non-overlapping ranges of the range type.
    Multirange(Box<PgType>),
    TstzRange,
    NumRange,
    DateRange,
    Int4Range,
    Int8Range,
    /// One-dimensional array of the element type (PostgreSQL arrays are not dimension-typed,
    /// so multi-dimensional values are exposed as nested lists of the same element).
    Array(Box<PgType>),
    /// A composite (row) type, by its quoted qualified name, e.g. `"public"."address"`.
    Composite(String),
    /// An enum type, by its quoted qualified name, e.g. `"public"."item_type"`.
    Enum(String),
    Other(String),
}

impl PgType {
    pub fn from_sql_type(type_name: &str) -> Self {
        match type_name.to_lowercase().as_str() {
            "uuid" => PgType::Uuid,
            "text" => PgType::Text,
            "varchar" | "character varying" => PgType::Varchar,
            "char" | "character" | "bpchar" => PgType::Char,
            "int2" | "smallint" => PgType::Int2,
            "int4" | "integer" => PgType::Int4,
            "int8" | "bigint" => PgType::Int8,
            "bool" | "boolean" => PgType::Bool,
            "float4" | "real" => PgType::Float4,
            "float8" | "double precision" => PgType::Float8,
            "numeric" | "decimal" => PgType::Numeric,
            "timestamp" | "timestamp without time zone" => PgType::Timestamp,
            "timestamptz" | "timestamp with time zone" => PgType::TimestampTz,
            "date" => PgType::Date,
            "time" | "time without time zone" => PgType::Time,
            "timetz" | "time with time zone" => PgType::TimeTz,
            "interval" => PgType::Interval,
            "bytea" => PgType::Bytea,
            "money" => PgType::Money,
            "inet" => PgType::Inet,
            "cidr" => PgType::Cidr,
            "macaddr" => PgType::MacAddr,
            "macaddr8" => PgType::MacAddr8,
            "vector" => PgType::Vector,
            "ltree" => PgType::Ltree,
            "tsvector" => PgType::TsVector,
            "geometry" => PgType::Geometry,
            "geography" => PgType::Geography,
            "tstzmultirange" => PgType::Multirange(Box::new(PgType::TstzRange)),
            "nummultirange" => PgType::Multirange(Box::new(PgType::NumRange)),
            "datemultirange" => PgType::Multirange(Box::new(PgType::DateRange)),
            "int4multirange" => PgType::Multirange(Box::new(PgType::Int4Range)),
            "int8multirange" => PgType::Multirange(Box::new(PgType::Int8Range)),
            "json" => PgType::Json,
            "jsonb" => PgType::Jsonb,
            "citext" => PgType::Citext,
            "tstzrange" => PgType::TstzRange,
            "numrange" => PgType::NumRange,
            "daterange" => PgType::DateRange,
            "int4range" => PgType::Int4Range,
            "int8range" => PgType::Int8Range,
            // `pg_type` names array types after their element with a leading underscore
            other => match other.strip_prefix('_') {
                Some(elem) if !elem.is_empty() => {
                    PgType::Array(Box::new(PgType::from_sql_type(elem)))
                }
                _ => PgType::Other(other.to_string()),
            },
        }
    }

    /// Type name a text-bound query parameter can be cast to so comparisons against
    /// this column use its native operators (and indexes). `None` for types without a
    /// safe unqualified cast: `json` (no equality operator) and user-defined types (schema
    /// not tracked). `citext` resolves through the connection's `search_path`, as writes do,
    /// so comparisons keep its case-insensitive equality.
    pub fn sql_cast(&self) -> Option<String> {
        let name = match self {
            PgType::Uuid => "uuid",
            PgType::Text => "text",
            PgType::Varchar => "varchar",
            PgType::Char => "bpchar",
            PgType::Int2 => "int2",
            PgType::Int4 => "int4",
            PgType::Int8 => "int8",
            PgType::Bool => "bool",
            PgType::Float4 => "float4",
            PgType::Float8 => "float8",
            PgType::Numeric => "numeric",
            PgType::Timestamp => "timestamp",
            PgType::TimestampTz => "timestamptz",
            PgType::Date => "date",
            PgType::Time => "time",
            PgType::TimeTz => "timetz",
            PgType::Interval => "interval",
            // Input values are converted to what these parse: `\x` hex, plain decimals
            PgType::Bytea => "bytea",
            PgType::Money => "money",
            PgType::Inet => "inet",
            PgType::Cidr => "cidr",
            PgType::MacAddr => "macaddr",
            PgType::MacAddr8 => "macaddr8",
            PgType::Vector => "vector",
            PgType::Ltree => "ltree",
            PgType::TsVector => "tsvector",
            PgType::Geometry => "geometry",
            PgType::Geography => "geography",
            PgType::Multirange(range) => {
                return range.sql_cast().map(|r| r.replace("range", "multirange"))
            }
            PgType::Jsonb => "jsonb",
            PgType::Citext => "citext",
            PgType::TstzRange => "tstzrange",
            PgType::NumRange => "numrange",
            PgType::DateRange => "daterange",
            PgType::Int4Range => "int4range",
            PgType::Int8Range => "int8range",
            PgType::Array(elem) => return elem.sql_cast().map(|e| format!("{e}[]")),
            PgType::Enum(name) => return Some(name.clone()),
            PgType::Json | PgType::Composite(_) | PgType::Other(_) => return None,
        };
        Some(name.to_string())
    }

    pub fn is_orderable(&self) -> bool {
        !matches!(
            self,
            PgType::Json
                | PgType::Jsonb
                | PgType::Array(_)
                | PgType::Composite(_)
                | PgType::Other(_)
                | PgType::Bytea
                | PgType::Vector
                | PgType::TsVector
                | PgType::Geometry
                | PgType::Geography
                | PgType::Multirange(_)
        )
    }

    /// PostGIS `geometry` or `geography`.
    pub fn is_spatial(&self) -> bool {
        matches!(self, PgType::Geometry | PgType::Geography)
    }

    /// The range type of a multirange type.
    pub fn multirange_of(&self) -> Option<&PgType> {
        match self {
            PgType::Multirange(range) => Some(range),
            _ => None,
        }
    }

    /// Numeric: summed and averaged (`money` is not: PostgreSQL has no `avg(money)`).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
        )
    }

    pub fn is_range(&self) -> bool {
        matches!(
            self,
            PgType::TstzRange
                | PgType::NumRange
                | PgType::DateRange
                | PgType::Int4Range
                | PgType::Int8Range
        )
    }

    pub fn is_array(&self) -> bool {
        matches!(self, PgType::Array(_))
    }

    /// Element type of a range type, and the GraphQL scalar its range types are named
    /// after (`IntRange`, `IntRangeInput`, `IntRangeFilter`, ...).
    pub fn range_element(&self) -> Option<(PgType, &'static str)> {
        match self {
            PgType::TstzRange => Some((PgType::TimestampTz, "Datetime")),
            PgType::DateRange => Some((PgType::Date, "Date")),
            PgType::NumRange => Some((PgType::Numeric, "Decimal")),
            PgType::Int4Range => Some((PgType::Int4, "Int")),
            PgType::Int8Range => Some((PgType::Int8, "BigInt")),
            _ => None,
        }
    }

    /// The range filter input of a range type (`IntRangeFilter`).
    pub fn range_filter_input_name(&self) -> Option<String> {
        self.range_element()
            .map(|(_, scalar)| format!("{scalar}RangeFilter"))
    }

    /// SQL turning a stored value `{v}` into its response value, for types whose JSON form
    /// needs more than a cast: non-finite floats (not valid GraphQL `Float`s) become null,
    /// `money` a decimal (its text depends on `lc_monetary`), `bytea` base64, `interval` an
    /// ISO 8601 duration, `vector` a list of floats.
    pub fn output_sql(&self) -> Option<&'static str> {
        Some(match self {
            PgType::Float4 | PgType::Float8 => {
                "CASE WHEN {v} IN ('NaN', 'Infinity', '-Infinity') THEN NULL ELSE {v} END"
            }
            PgType::Numeric => {
                "CASE WHEN {v} IN ('NaN', 'Infinity', '-Infinity') THEN NULL ELSE {v}::text END"
            }
            PgType::Money => "{v}::numeric::text",
            PgType::Bytea => "translate(encode({v}, 'base64'), chr(10), '')",
            PgType::Vector => "{v}::text::json",
            PgType::Interval => INTERVAL_ISO_8601,
            PgType::Geometry | PgType::Geography => "ST_AsGeoJSON({v})::json",
            _ => return None,
        })
    }

    pub fn element_cast(&self) -> Option<&'static str> {
        match self {
            PgType::TstzRange => Some("timestamptz"),
            PgType::DateRange => Some("date"),
            PgType::NumRange => Some("numeric"),
            PgType::Int4Range => Some("int4"),
            PgType::Int8Range => Some("int8"),
            _ => None,
        }
    }

    /// Type to cast a text-bound value to when writing it (INSERT values, UPDATE SET).
    /// Unlike [`sql_cast`](Self::sql_cast) this covers every type: an uncast text parameter
    /// fails for any non-text column anyway. User-defined types resolve through the
    /// connection's `search_path`; composites are written from JSON (see `ColumnValue`).
    pub fn assignment_cast(&self) -> String {
        match (self.sql_cast(), self) {
            (Some(cast), _) => cast,
            (None, PgType::Json) => "json".to_string(),
            (None, PgType::Array(elem)) => format!("{}[]", elem.assignment_cast()),
            (None, PgType::Composite(name)) => name.clone(),
            (None, PgType::Other(name)) => name.clone(),
            (None, _) => "text".to_string(),
        }
    }

    /// GraphQL scalar of a value of this type (of each element, for arrays).
    pub fn to_graphql_scalar(&self) -> &'static str {
        match self {
            PgType::Uuid => "UUID",
            PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext | PgType::Ltree => {
                "String"
            }
            PgType::Int2 | PgType::Int4 => "Int",
            PgType::Int8 => "BigInt",
            PgType::Bool => "Boolean",
            PgType::Float4 | PgType::Float8 => "Float",
            PgType::Numeric => "Decimal",
            // Catalog-aware callers use the enum's GraphQL type (`DatabaseCatalog::graphql_type`)
            PgType::Enum(_) => "String",
            PgType::Timestamp => "LocalDatetime",
            PgType::TimestampTz => "Datetime",
            PgType::Date => "Date",
            PgType::Time => "Time",
            PgType::TimeTz => "TimeTz",
            PgType::Interval => "Interval",
            PgType::Bytea => "Base64",
            PgType::Money => "Decimal",
            PgType::Inet => "InternetAddress",
            PgType::Cidr => "CidrAddress",
            PgType::MacAddr | PgType::MacAddr8 => "MacAddress",
            PgType::Vector => "Float",
            PgType::TsVector => "String",
            PgType::Geometry | PgType::Geography => "GeoJSON",
            PgType::Multirange(range) => range.to_graphql_scalar(),
            PgType::Json | PgType::Jsonb | PgType::Composite(_) => "JSON",
            PgType::Array(elem) => elem.to_graphql_scalar(),
            PgType::TstzRange => "DatetimeRange",
            PgType::NumRange => "DecimalRange",
            PgType::DateRange => "DateRange",
            PgType::Int4Range => "IntRange",
            PgType::Int8Range => "BigIntRange",
            PgType::Other(_) => "String",
        }
    }

    /// GraphQL type of a (nullable) value of this type: the scalar, or a list of it.
    pub fn graphql_type(&self) -> String {
        match self {
            PgType::Array(elem) => format!("[{}]", elem.graphql_type()),
            PgType::Vector => "[Float!]".to_string(),
            PgType::Multirange(range) => format!("[{}!]", range.graphql_type()),
            other => other.to_graphql_scalar().to_string(),
        }
    }

    /// Cast applied when a value is put into the JSON response, so it arrives as the
    /// declared GraphQL type: `bigint`/`numeric` as strings (JSON numbers lose precision
    /// past 2^53 in most clients) and types without a JSON form of their own as their text.
    pub fn output_cast(&self) -> Option<String> {
        match self {
            PgType::Int8 | PgType::Numeric | PgType::Other(_) | PgType::TsVector => {
                Some("text".to_string())
            }
            PgType::Array(elem) => elem.output_cast().map(|c| format!("{c}[]")),
            _ => None,
        }
    }

    /// Whether values bind as a PostgreSQL array literal built from a JSON list.
    pub fn binds_as_array_literal(&self) -> bool {
        matches!(self, PgType::Array(elem) if !matches!(**elem, PgType::Composite(_)))
    }
}

/// GraphQL enum value for a database label: `in-progress` -> `IN_PROGRESS`,
/// `camelCase` -> `CAMEL_CASE`, `1st` -> `_1ST`.
pub fn enum_value_name(label: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_uppercase());
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    let out = out.trim_end_matches('_').to_string();
    match out.chars().next() {
        None => "_EMPTY".to_string(),
        Some(c) if c.is_ascii_digit() => format!("_{out}"),
        _ => out,
    }
}

/// One value of a GraphQL enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumValue {
    /// Database value.
    pub label: String,
    /// GraphQL enum value.
    pub name: String,
}

/// A domain exposed as a GraphQL scalar of its own (`CREATE DOMAIN email AS text ...` →
/// `scalar Email`). Its values are those of its base type, checked and converted like them;
/// the domain's constraints are checked by PostgreSQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainMetadata {
    /// How columns refer to it (`ColumnMetadata::domain`): the quoted qualified type name.
    pub key: String,
    /// Name in the config's `types` section: the domain's name.
    pub config_name: String,
    pub type_name: String,
    pub description: Option<String>,
    pub base: PgType,
}

impl PgType {
    /// Types a domain can be a scalar over: those whose values are one GraphQL scalar
    /// (not lists, ranges, composites or enums).
    pub fn is_scalar_base(&self) -> bool {
        !matches!(
            self,
            PgType::Array(_)
                | PgType::Composite(_)
                | PgType::Enum(_)
                | PgType::Other(_)
                | PgType::Vector
                | PgType::Multirange(_)
        ) && !self.is_range()
    }
}

/// A GraphQL enum type: a PostgreSQL enum type, or a text column whose values a `CHECK`
/// constraint closes (`CHECK (status IN ('draft', 'active'))`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumTypeMetadata {
    /// How columns refer to it (`ColumnMetadata::enum_key`): the quoted qualified type name,
    /// or `check:<table>.<column>`.
    pub key: String,
    /// Name in the config's `types` section: the type name, or `<table>.<column>`.
    pub config_name: String,
    pub type_name: String,
    pub description: Option<String>,
    pub values: Vec<EnumValue>,
}

impl EnumTypeMetadata {
    pub fn label_of(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.label.as_str())
    }

    /// Labels and GraphQL values differ, so values need mapping in and out.
    pub fn needs_mapping(&self) -> bool {
        self.values.iter().any(|v| v.label != v.name)
    }
}

/// One attribute of a composite type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeAttribute {
    pub name: String,
    pub pg_type: PgType,
    pub field_name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub enum_key: Option<String>,
    #[serde(default)]
    pub limits: TypeLimits,
}

/// A composite type used by some column: a GraphQL object type and a matching input type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositeTypeMetadata {
    /// Quoted qualified name, as in `PgType::Composite`.
    pub key: String,
    /// Name in the config's `types` section.
    pub config_name: String,
    pub type_name: String,
    pub description: Option<String>,
    pub attributes: Vec<CompositeAttribute>,
}

impl CompositeTypeMetadata {
    pub fn input_type_name(&self) -> String {
        format!("{}Input", self.type_name)
    }

    pub fn attribute_by_field(&self, field_name: &str) -> Option<&CompositeAttribute> {
        self.attributes.iter().find(|a| a.field_name == field_name)
    }
}

/// Converts a JSON list into a PostgreSQL array literal (`{"a","b",NULL}`), the text form a
/// text-bound parameter needs before a `::text[]`-style cast. Non-list values pass through.
pub fn pg_array_literal(value: &serde_json::Value) -> serde_json::Value {
    fn element(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Null => "NULL".to_string(),
            serde_json::Value::Array(items) => format!(
                "{{{}}}",
                items.iter().map(element).collect::<Vec<_>>().join(",")
            ),
            serde_json::Value::String(s) => quote(s),
            other => quote(&other.to_string()),
        }
    }
    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
    match value {
        serde_json::Value::Array(_) => serde_json::Value::String(element(value)),
        other => other.clone(),
    }
}

/// Limits a type modifier puts on values (`varchar(20)`, `numeric(10, 2)`); for arrays, on
/// each element. Writes are checked against them before any SQL runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeLimits {
    /// Most characters of a `varchar(n)` / `char(n)` value.
    #[serde(default)]
    pub max_length: Option<u32>,
    /// `(precision, scale)` of a `numeric(p, s)` value: at most `p` significant digits,
    /// rounded to `s` decimal places (a negative scale rounds left of the point).
    #[serde(default)]
    pub precision: Option<(u32, i32)>,
}

impl TypeLimits {
    /// Limits of a value of `pg_type` declared with type modifier `typmod` (`-1`: none).
    pub fn from_typmod(pg_type: &PgType, typmod: i32) -> Self {
        const VARHDRSZ: i32 = 4;
        if typmod < VARHDRSZ {
            return Self::default();
        }
        match pg_type {
            PgType::Array(elem) => Self::from_typmod(elem, typmod),
            PgType::Varchar | PgType::Char => Self {
                max_length: Some((typmod - VARHDRSZ) as u32),
                precision: None,
            },
            PgType::Numeric => {
                let t = typmod - VARHDRSZ;
                // `NUMERIC_TYPMOD_SCALE`: an 11-bit signed scale (negative since PostgreSQL 15)
                let scale = ((t & 0x7ff) ^ 1024) - 1024;
                Self {
                    max_length: None,
                    precision: Some((((t >> 16) & 0xffff) as u32, scale)),
                }
            }
            _ => Self::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.max_length.is_none() && self.precision.is_none()
    }

    /// Sentence documenting the limits, e.g. "At most 20 characters."
    pub fn describe(&self) -> Option<String> {
        if let Some(n) = self.max_length {
            return Some(format!(
                "At most {n} character{}.",
                if n == 1 { "" } else { "s" }
            ));
        }
        let (p, s) = self.precision?;
        Some(match s {
            0 => format!("A whole number of at most {p} digits."),
            s if s > 0 => format!("At most {p} digits, {s} of them after the decimal point (rounded to {s} decimal places)."),
            s => format!("At most {p} significant digits, rounded to a multiple of 10^{}.", -s),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMetadata {
    pub name: String,
    pub pg_type: PgType,
    pub is_nullable: bool,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
    pub is_generated: bool,
    pub description: Option<String>,
    /// Length and precision limits of the column's type modifier.
    #[serde(default)]
    pub limits: TypeLimits,
    /// Expression of a generated column (`GENERATED ALWAYS AS (...) STORED`).
    #[serde(default)]
    pub generation: Option<String>,
    /// Why the field is deprecated (`@deprecated` in its comment, or the config).
    #[serde(default)]
    pub deprecation: Option<String>,
    /// GraphQL field name (camelCase of the column name unless configured).
    #[serde(default)]
    pub field_name: String,
    /// Hidden from every GraphQL type, input and argument (still usable for joins and keys).
    #[serde(default)]
    pub omit: bool,
    /// Read-only column: visible in queries, but omitted from create and update inputs.
    #[serde(default)]
    pub readonly: bool,
    /// Enum type of the column (or of its elements), see `EnumTypeMetadata::key`.
    #[serde(default)]
    pub enum_key: Option<String>,
    /// The domain the column is declared with (quoted qualified name), when it is exposed
    /// as a scalar (see `DatabaseCatalog::domains`); `pg_type` is its base type.
    #[serde(default)]
    pub domain: Option<String>,
}

impl ColumnMetadata {
    /// Name of the column in `<Type>OrderBy` enums, e.g. `createdAt` -> `CREATED_AT`.
    pub fn order_by_name(&self) -> String {
        to_snake_case(&self.field_name).to_uppercase()
    }
}

/// A foreign key: `columns` of the owning table reference `foreign_columns` (same order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyMetadata {
    pub constraint_name: String,
    pub columns: Vec<String>,
    pub foreign_schema: String,
    pub foreign_table: String,
    pub foreign_columns: Vec<String>,
    /// Declared in the engine config rather than by a database constraint (e.g. on a view).
    #[serde(default)]
    pub is_virtual: bool,
    /// Configured name of the to-one relation field on the owning type.
    #[serde(default)]
    pub field_name: Option<String>,
    /// Configured name of the to-many relation field on the referenced type (its connection
    /// field gets a `Connection` suffix).
    #[serde(default)]
    pub reverse_field_name: Option<String>,
    /// Hide both relation fields.
    #[serde(default)]
    pub omit: bool,
    /// Hide only the to-many relation fields on the referenced type.
    #[serde(default)]
    pub omit_reverse: bool,
    /// A PostgreSQL 18 temporal foreign key (`FOREIGN KEY (a, PERIOD valid_at)`): its last
    /// columns are ranges, and a row refers to the referenced rows whose range overlaps its
    /// own (not to one with an equal range).
    #[serde(default)]
    pub period: bool,
}

/// `(column, other column)`: a column of one table and the one it matches in another.
pub type ColumnPair = (String, String);

impl ForeignKeyMetadata {
    /// `(column, foreign column)` pairs compared by equality, and the `(column, foreign
    /// column)` range pair of a temporal foreign key.
    pub fn split_period(&self) -> (Vec<ColumnPair>, Option<ColumnPair>) {
        let mut pairs: Vec<(String, String)> = self
            .columns
            .iter()
            .cloned()
            .zip(self.foreign_columns.iter().cloned())
            .collect();
        let period = if self.period { pairs.pop() } else { None };
        (pairs, period)
    }

    /// `(column, foreign_column)` for a single-column foreign key.
    pub fn single(&self) -> Option<(&str, &str)> {
        match (self.columns.as_slice(), self.foreign_columns.as_slice()) {
            ([c], [f]) => Some((c.as_str(), f.as_str())),
            _ => None,
        }
    }
}

/// What kind of relation (`pg_class.relkind`) a catalog entity is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationKind {
    #[default]
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    pub fn from_relkind(relkind: &str) -> Option<Self> {
        Some(match relkind {
            "r" => Self::Table,
            "p" => Self::PartitionedTable,
            "v" => Self::View,
            "m" => Self::MaterializedView,
            "f" => Self::ForeignTable,
            _ => return None,
        })
    }
}

/// Root query and mutation fields generated for an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Operations {
    /// Single row by primary key (and by each unique key).
    pub read: bool,
    pub list: bool,
    pub connection: bool,
    pub create: bool,
    /// Update and delete one row by primary key (and by each unique key).
    pub update: bool,
    pub delete: bool,
    /// Insert several rows (`create<Types>`).
    pub create_many: bool,
    /// Update and delete the rows matching a filter (`update<Types>`, `delete<Types>`).
    pub update_many: bool,
    pub delete_many: bool,
    /// `onConflict` on creates: insert or update / keep the row holding a unique key.
    pub upsert: bool,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            read: true,
            list: true,
            connection: true,
            create: true,
            update: true,
            delete: true,
            create_many: true,
            update_many: true,
            delete_many: true,
            upsert: true,
        }
    }
}

/// A unique index of a table on plain columns (no expressions, no predicate): the primary key
/// or a unique constraint/index. Rows can be addressed by it and it can arbitrate
/// `INSERT ... ON CONFLICT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueKey {
    /// Index name.
    pub name: String,
    /// Columns in index order.
    pub columns: Vec<String>,
    pub is_primary: bool,
}

/// What a constraint enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintKind {
    PrimaryKey,
    /// A unique constraint, or a unique index without one (possibly partial or on
    /// expressions).
    Unique,
    ForeignKey,
    Check,
    Exclusion,
}

/// A constraint of a table, and the error code a violation of it is reported with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstraintMetadata {
    /// Constraint (or unique index) name, as PostgreSQL reports it in errors.
    pub name: String,
    pub kind: ConstraintKind,
    /// Columns it constrains (for expression indexes, the columns the expressions read).
    pub columns: Vec<String>,
    /// Definition as PostgreSQL prints it, e.g. `CHECK ((score >= 0))`.
    pub definition: String,
    /// Foreign keys: the referenced `(schema, table)`.
    #[serde(default)]
    pub references: Option<(String, String)>,
    /// A PostgreSQL 18 temporal key or foreign key (`WITHOUT OVERLAPS`, `PERIOD`): its last
    /// column is a range compared by overlap.
    #[serde(default)]
    pub period: bool,
    /// Error code of a violation (`EMAIL_TAKEN`, `TEAM_NOT_FOUND`), a value of the table's
    /// `<Type>ErrorCode` enum. Assigned once names are final.
    #[serde(default)]
    pub code: String,
    /// Foreign keys: the code a write of the referenced table fails with while rows still
    /// refer to it (`REFERENCED_BY_PLAYERS`), a value of the referenced table's enum.
    #[serde(default)]
    pub referenced_code: String,
}

impl ConstraintMetadata {
    /// The expression of a `CHECK` constraint: `score >= 0` for `CHECK ((score >= 0))`.
    pub fn check_expression(&self) -> Option<String> {
        let inner = self.definition.strip_prefix("CHECK ")?;
        let inner = inner.strip_suffix(" NOT VALID").unwrap_or(inner);
        let mut expr = inner.trim();
        // Outer parentheses PostgreSQL adds, as long as they enclose the whole expression
        while let Some(stripped) = expr.strip_prefix('(').and_then(|e| e.strip_suffix(')')) {
            if !balanced(stripped) {
                break;
            }
            expr = stripped.trim();
        }
        Some(expr.to_string())
    }
}

/// Whether the parentheses of `s` (outside string literals) balance.
fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    for c in s.chars() {
        match c {
            '\'' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableMetadata {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub kind: RelationKind,
    pub columns: Vec<ColumnMetadata>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKeyMetadata>,
    pub description: Option<String>,
    /// GraphQL object type name (singular PascalCase of the table name unless configured).
    #[serde(default)]
    pub type_name: String,
    /// Root query field names.
    #[serde(default)]
    pub list_field: String,
    #[serde(default)]
    pub single_field: String,
    #[serde(default)]
    pub connection_field: String,
    /// Legacy alias of the connection field (`allUsersConnection`), dropped when the
    /// connection field is renamed.
    #[serde(default)]
    pub all_connection_field: Option<String>,
    #[serde(default)]
    pub operations: Operations,
    /// Unique indexes on plain columns, the primary key first (only indexes the database
    /// has: a key declared in the config cannot arbitrate `ON CONFLICT`).
    #[serde(default)]
    pub unique_keys: Vec<UniqueKey>,
    /// Primary key, unique, foreign key, check and exclusion constraints (and unique
    /// indexes), in catalog order.
    #[serde(default)]
    pub constraints: Vec<ConstraintMetadata>,
    /// A column a trigger maintains as the row's version (configured: the database cannot
    /// declare what a column means); rows are versioned by `xmin` without one.
    #[serde(default)]
    pub version_column: Option<String>,
    /// Why the type's root fields (and relation fields leading to it) are deprecated.
    #[serde(default)]
    pub deprecation: Option<String>,
    /// Apollo Federation key fields override (e.g. `"id"` or `"tenantId userId"`).
    #[serde(default)]
    pub federation_key: Option<String>,
    /// Mark entity as @shareable in Apollo Federation.
    #[serde(default)]
    pub federation_shareable: bool,
}

/// The key of a table with a PostgreSQL 18 temporal key (`PRIMARY KEY (id, valid_at
/// WITHOUT OVERLAPS)`): each entity (`id`) has versions over non-overlapping periods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodKey {
    /// Columns identifying the entity: the key without its period.
    pub entity: Vec<String>,
    /// The range column versions are valid over.
    pub period: String,
}

impl TableMetadata {
    pub fn get_column(&self, name: &str) -> Option<&ColumnMetadata> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The temporal key: the primary key when it has a period, else the first unique
    /// constraint with one.
    pub fn period_key(&self) -> Option<PeriodKey> {
        let con = self
            .constraints
            .iter()
            .filter(|c| {
                c.period && matches!(c.kind, ConstraintKind::PrimaryKey | ConstraintKind::Unique)
            })
            .min_by_key(|c| c.kind != ConstraintKind::PrimaryKey)?;
        let (period, entity) = con.columns.split_last()?;
        let usable = self
            .get_column(period)
            .is_some_and(|c| c.pg_type.is_range() && !c.omit);
        usable.then(|| PeriodKey {
            entity: entity.to_vec(),
            period: period.clone(),
        })
    }

    /// Rows have a version clients can pass back as `expectedVersion`: tables have `xmin`,
    /// other relations need a configured version column. Rows are addressed by key.
    pub fn has_row_version(&self) -> bool {
        !self.primary_key.is_empty()
            && (self.version_column.is_some()
                || matches!(
                    self.kind,
                    RelationKind::Table
                        | RelationKind::PartitionedTable
                        | RelationKind::MaterializedView
                ))
    }

    /// GraphQL scalar of the `asOf` argument of fields reading this table's rows: the
    /// element type of its temporal key's period.
    pub fn as_of_scalar(&self) -> Option<&'static str> {
        let key = self.period_key()?;
        Some(self.get_column(&key.period)?.pg_type.range_element()?.1)
    }

    /// Column (or system column) holding the row version.
    pub fn row_version_column(&self) -> &str {
        self.version_column.as_deref().unwrap_or("xmin")
    }

    /// Columns exposed in GraphQL.
    pub fn visible_columns(&self) -> impl Iterator<Item = &ColumnMetadata> {
        self.columns.iter().filter(|c| !c.omit)
    }

    /// Values of the `<Type>GroupBy` enum: `(value, column, date_trunc unit)`. Every exposed
    /// orderable column (`STATUS`), dates and timestamps truncated to a unit of time
    /// (`CREATED_AT_TRUNCATED_TO_MONTH`), and timestamps in bins of a chosen width
    /// (`CREATED_AT_BINNED`, unit `bin`).
    pub fn group_by_values(&self) -> Vec<(String, &ColumnMetadata, Option<&'static str>)> {
        let mut out: Vec<(String, &ColumnMetadata, Option<&'static str>)> = Vec::new();
        for col in self.visible_columns().filter(|c| c.pg_type.is_orderable()) {
            let name = col.order_by_name();
            let units: &[&'static str] = match col.pg_type {
                PgType::Timestamp | PgType::TimestampTz => {
                    &["minute", "hour", "day", "week", "month", "quarter", "year"]
                }
                PgType::Date => &["week", "month", "quarter", "year"],
                _ => &[],
            };
            // Timestamps also fall into bins of any width (`date_bin`): `<COLUMN>_BINNED`
            let binned = matches!(col.pg_type, PgType::Timestamp | PgType::TimestampTz)
                .then(|| (format!("{name}_BINNED"), Some("bin")));
            let values = std::iter::once((name.clone(), None))
                .chain(units.iter().map(|u| {
                    (
                        format!("{name}_TRUNCATED_TO_{}", u.to_uppercase()),
                        Some(*u),
                    )
                }))
                .chain(binned);
            for (value, unit) in values {
                if !out.iter().any(|(v, _, _)| *v == value) {
                    out.push((value, col, unit));
                }
            }
        }
        out
    }

    /// Exposed column behind GraphQL field `field_name`.
    pub fn column_by_field(&self, field_name: &str) -> Option<&ColumnMetadata> {
        self.visible_columns().find(|c| c.field_name == field_name)
    }

    /// A single-column primary key makes the type a Relay `Node`.
    pub fn implements_node(&self) -> bool {
        self.primary_key.len() == 1
    }

    /// Primary key columns, or `None` for relations without a key.
    pub fn key_columns(&self) -> Option<Vec<&ColumnMetadata>> {
        if self.primary_key.is_empty() {
            return None;
        }
        self.primary_key
            .iter()
            .map(|pk| self.get_column(pk))
            .collect()
    }

    /// Key fields string for Apollo Federation @key(fields: "...").
    pub fn federation_key_fields(&self) -> Option<String> {
        if let Some(key) = &self.federation_key {
            return Some(key.clone());
        }
        if self.primary_key.is_empty() {
            return None;
        }
        let fields: Vec<String> = self
            .primary_key
            .iter()
            .filter_map(|pk| self.get_column(pk).map(|c| c.field_name.clone()))
            .collect();
        if fields.is_empty() {
            None
        } else {
            Some(fields.join(" "))
        }
    }

    /// Whether this table generates an Apollo Federation entity type.
    pub fn is_federated_entity(&self) -> bool {
        self.federation_key_fields().is_some()
    }

    /// Columns of `<Type>` create and update inputs.
    pub fn writable_columns(&self) -> impl Iterator<Item = &ColumnMetadata> {
        self.visible_columns()
            .filter(|c| !c.is_generated && !c.readonly)
    }

    /// Exposed columns of a unique key, or `None` if one is hidden.
    fn exposed_key_columns(&self, key: &UniqueKey) -> Option<Vec<&ColumnMetadata>> {
        key.columns
            .iter()
            .map(|c| self.get_column(c).filter(|c| !c.omit))
            .collect()
    }

    /// Unique keys other than the primary key that rows can be looked up by
    /// (`userByEmail`): every column exposed and comparable by equality, and not named like
    /// the `input` argument its update field also takes.
    pub fn lookup_keys(&self) -> impl Iterator<Item = (&UniqueKey, Vec<&ColumnMetadata>)> {
        let comparable = |c: &ColumnMetadata| {
            !matches!(c.pg_type, PgType::Json | PgType::Composite(_)) && c.field_name != "input"
        };
        self.unique_keys
            .iter()
            // The primary key has its lookup already, also when it is declared in
            // `graphpg.yaml` and a unique index covers the same columns
            .filter(|k| !k.is_primary && k.columns != self.primary_key)
            .filter_map(move |k| {
                let cols = self.exposed_key_columns(k)?;
                cols.iter().all(|c| comparable(c)).then_some((k, cols))
            })
    }

    /// Unique keys an insert can conflict on (`onConflict: { key: ... }`): every column
    /// writable, so a create input can carry the key, and not composite (rows are read back
    /// by the key's text values).
    pub fn conflict_keys(&self) -> impl Iterator<Item = (&UniqueKey, Vec<&ColumnMetadata>)> {
        self.unique_keys.iter().filter_map(move |k| {
            let cols = self.exposed_key_columns(k)?;
            cols.iter()
                .all(|c| !c.is_generated && !matches!(c.pg_type, PgType::Composite(_)))
                .then_some((k, cols))
        })
    }

    /// `onConflict` is generated for creates.
    pub fn has_upsert(&self) -> bool {
        self.operations.upsert && self.conflict_keys().next().is_some()
    }

    /// Keys a row can be identified by, as `(field name, column)` lists: the primary key
    /// (`id` for a Relay node, a Relay ID or the raw value), then each unique lookup key.
    /// Together they make the fields of `<Type>KeyInput`.
    pub fn row_keys(&self) -> Vec<Vec<(String, &ColumnMetadata)>> {
        let mut keys = Vec::new();
        if let Some(pk) = self.key_columns() {
            if self.implements_node() {
                keys.push(vec![("id".to_string(), pk[0])]);
            } else {
                keys.push(pk.into_iter().map(|c| (c.field_name.clone(), c)).collect());
            }
        }
        keys.extend(self.lookup_keys().map(|(_, cols)| {
            cols.into_iter()
                .map(|c| (c.field_name.clone(), c))
                .collect()
        }));
        keys
    }

    /// Rows can be created from a `Create<Type>Input` (a nested `create`).
    pub fn is_insertable(&self) -> bool {
        self.operations.create && self.writable_columns().next().is_some()
    }

    /// Rows can be found by a `<Type>KeyInput` (a nested `connect`).
    pub fn is_connectable(&self) -> bool {
        !self.primary_key.is_empty() || self.lookup_keys().next().is_some()
    }

    /// Plural of the type name: `Forums`, `Categories`, `Addresses`.
    pub fn plural_type_name(&self) -> String {
        pluralize(&self.type_name)
    }
}

/// `By<Field>And<Field>` suffix of the fields addressing a row by a unique key.
pub fn unique_key_suffix(columns: &[&ColumnMetadata]) -> String {
    let names: Vec<String> = columns
        .iter()
        .map(|c| to_pascal_case(&c.field_name))
        .collect();
    format!("By{}", names.join("And"))
}

/// `<Type>ConflictKey` value of a unique key: `PRIMARY_KEY`, or its columns' `orderBy`
/// names joined by `_AND_` (`ORGANIZATION_ID_AND_SLUG`).
pub fn conflict_key_value(key: &UniqueKey, columns: &[&ColumnMetadata]) -> String {
    if key.is_primary {
        return "PRIMARY_KEY".to_string();
    }
    columns
        .iter()
        .map(|c| c.order_by_name())
        .collect::<Vec<_>>()
        .join("_AND_")
}

/// English plural of a PascalCase name (of its last word).
pub fn pluralize(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let consonant_y = lower.ends_with('y')
        && !lower.ends_with("ay")
        && !lower.ends_with("ey")
        && !lower.ends_with("oy")
        && !lower.ends_with("uy");
    if consonant_y {
        format!("{}ies", &name[..name.len() - 1])
    } else if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        format!("{name}es")
    } else {
        format!("{name}s")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionArgument {
    pub name: String,
    pub pg_type: PgType,
    #[serde(default)]
    pub has_default: bool,
}

impl FunctionArgument {
    /// GraphQL argument name: camelCase without the conventional `_`/`p_` prefix.
    pub fn graphql_name(&self) -> String {
        to_camel_case(self.name.trim_start_matches('_'))
    }

    /// The argument as a column, so it is typed, and its values converted, like one.
    pub fn as_column(&self) -> ColumnMetadata {
        ColumnMetadata {
            name: self.name.clone(),
            enum_key: pg_type_enum_key(&self.pg_type),
            pg_type: self.pg_type.clone(),
            is_nullable: true,
            is_primary_key: false,
            default_value: None,
            is_generated: false,
            description: None,
            limits: TypeLimits::default(),
            generation: None,
            deprecation: None,
            field_name: self.graphql_name(),
            omit: false,
            readonly: false,
            domain: None,
        }
    }

    /// Takes keys: a UUID, or named `*_id`. Typed `ID` so it accepts Relay IDs too.
    pub fn is_key_like(&self) -> bool {
        self.pg_type == PgType::Uuid
            || (self.name.ends_with("_id") && !matches!(self.pg_type, PgType::Array(_)))
    }
}

/// Enum type key of a value of `pg_type` (or of its elements).
pub fn pg_type_enum_key(pg_type: &PgType) -> Option<String> {
    match pg_type {
        PgType::Enum(key) => Some(key.clone()),
        PgType::Array(elem) => pg_type_enum_key(elem),
        _ => None,
    }
}

/// What a function returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum FunctionReturn {
    /// Columns of its own (`RETURNS TABLE (...)`, or several `OUT` parameters): a
    /// `<Func>Row` type from `return_columns`.
    #[default]
    Record,
    /// Rows of a table or view (`RETURNS [SETOF] users`): its GraphQL type.
    Table { schema: String, table: String },
    /// A value: base type, enum, composite type or array.
    Value(PgType),
    /// `RETURNS void`.
    Void,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionColumn {
    pub name: String,
    pub pg_type: PgType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionMetadata {
    pub schema: String,
    pub name: String,
    pub arguments: Vec<FunctionArgument>,
    /// Columns of a `Record` return.
    pub return_columns: Vec<FunctionColumn>,
    #[serde(default)]
    pub returns: FunctionReturn,
    /// Returns a set of rows or values (`SETOF`, `RETURNS TABLE`, or an array of rows).
    #[serde(default)]
    pub returns_set: bool,
    /// Returns its rows as an array (`RETURNS users[]`): they are read with `unnest`.
    #[serde(default)]
    pub returns_array: bool,
    /// `VOLATILE`: it may change data, so it is a mutation.
    #[serde(default)]
    pub volatile: bool,
    /// Schema of `parent_table`, when not the function's own.
    #[serde(default)]
    pub parent_schema: Option<String>,
    /// Table the function is mounted on as a field, its rows supplying `parent_arguments`.
    pub parent_table: Option<String>,
    pub description: Option<String>,
    /// Root Query field name.
    #[serde(default)]
    pub field_name: String,
    /// No root Query field.
    #[serde(default)]
    pub omit_root: bool,
    /// Field name on the parent table's type.
    #[serde(default)]
    pub mounted_field_name: String,
    /// `(argument, parent column)`: arguments filled from the parent row when mounted.
    #[serde(default)]
    pub parent_arguments: Vec<(String, String)>,
    /// The first argument takes the row of `parent_table` itself: a computed column, a
    /// field of that table's type.
    #[serde(default)]
    pub row_argument: bool,
    /// Why the function's fields are deprecated.
    #[serde(default)]
    pub deprecation: Option<String>,
}

impl FunctionMetadata {
    /// Whether the function is mounted as a field of `table`.
    pub fn is_mounted_on(&self, table: &TableMetadata) -> bool {
        self.parent_table.as_deref() == Some(table.name.as_str())
            && self.parent_schema.as_deref().unwrap_or(&self.schema) == table.schema
    }

    pub fn row_type_name(&self) -> String {
        format!("{}Row", to_pascal_case(&self.name))
    }

    /// Returns its own columns, as a `<Func>Row` type (and so can be mounted on a table).
    pub fn returns_record(&self) -> bool {
        self.returns == FunctionReturn::Record
    }

    /// Parent column bound to argument `arg` when the function is mounted.
    pub fn parent_column_for(&self, arg: &str) -> Option<&str> {
        self.parent_arguments
            .iter()
            .find(|(a, _)| a == arg)
            .map(|(_, c)| c.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExclusiveArcMetadata {
    pub constraint_name: String,
    pub table_name: String,
    pub parent_column: String,
    pub parent_table: String,
    pub target_columns: Vec<String>,
    pub target_tables: Vec<String>,
    /// Schema of each target table (parallel to `target_tables`). Empty in old serialized catalogs.
    #[serde(default)]
    pub target_schemas: Vec<String>,
    /// Column of each target table referenced by the arc FK (parallel to `target_tables`).
    #[serde(default)]
    pub target_key_columns: Vec<String>,
    /// GraphQL type name of each target table (parallel to `target_tables`).
    #[serde(default)]
    pub target_type_names: Vec<String>,
    pub union_name: String,
}

impl ExclusiveArcMetadata {
    /// `(arc_column, target_schema, target_table, target_key_column)` for every arc branch.
    pub fn branches<'a>(
        &'a self,
        default_schema: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str, &'a str, &'a str)> + 'a {
        self.target_columns
            .iter()
            .zip(&self.target_tables)
            .enumerate()
            .map(move |(i, (col, table))| {
                let schema = self
                    .target_schemas
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or(default_schema);
                let key = self
                    .target_key_columns
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or("id");
                (col.as_str(), schema, table.as_str(), key)
            })
    }

    /// GraphQL type name of branch `i`'s target table.
    pub fn target_type_name(&self, i: usize) -> String {
        match self.target_type_names.get(i) {
            Some(name) => name.clone(),
            None => table_to_type_name(&self.target_tables[i]),
        }
    }
}

/// A many-to-many relation from `source_table` to `target_table` through a junction table
/// holding a foreign key to each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManyToManyMetadata {
    #[serde(default)]
    pub source_schema: String,
    pub source_table: String,
    #[serde(default)]
    pub target_schema: String,
    pub target_table: String,
    pub junction_table: String,
    pub junction_schema: String,
    /// `(junction column, source column)` pairs.
    pub junction_source: Vec<(String, String)>,
    /// `(junction column, target column)` pairs.
    pub junction_target: Vec<(String, String)>,
    pub field_name: String,
    pub via_field_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchIndexKind {
    FuzzyTrgm,
    Vector {
        opclass: String,
    },
    Bm25,
    /// A `tsvector` document: a column, or the expression of an expression index
    /// (`to_tsvector('english', title || ' ' || body)`).
    FullText {
        /// SQL of the document, over the table's columns (unqualified).
        document: String,
        /// Text search configuration the document is built with (`'english'::regconfig`);
        /// queries use it too (else `default_text_search_config`).
        config: Option<String>,
        /// SQL of the text the document is built from, for highlights; `None` when unknown
        /// (e.g. a column a trigger maintains).
        source: Option<String>,
    },
    /// A GiST (or SP-GiST) index on a PostGIS column: nearest-neighbour ordering (`<->`).
    Spatial,
}

/// A way of searching a table, each a root query field of its own (`usersFuzzySearch`)
/// returning the best matches, without pagination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchMethod {
    /// Trigram similarity (`pg_trgm`).
    Fuzzy,
    /// Vector distance (`pgvector`).
    Semantic,
    /// BM25 full-text ranking (ParadeDB).
    Bm25,
    /// Vector and lexical rankings fused by reciprocal rank.
    Hybrid,
    /// Full-text search (`tsvector` documents): `websearch_to_tsquery`, ranked by `ts_rank`.
    FullText,
    /// Nearest geometries first (PostGIS `<->`).
    Nearest,
}

impl SearchMethod {
    pub const ALL: [SearchMethod; 6] = [
        Self::Fuzzy,
        Self::Semantic,
        Self::Bm25,
        Self::Hybrid,
        Self::FullText,
        Self::Nearest,
    ];

    /// Suffix of the method's root field, after the table's list field.
    pub fn field_suffix(self) -> &'static str {
        match self {
            Self::Fuzzy => "FuzzySearch",
            Self::Semantic => "SemanticSearch",
            Self::Bm25 => "Bm25Search",
            Self::Hybrid => "HybridSearch",
            Self::FullText => "FullTextSearch",
            Self::Nearest => "NearestSearch",
        }
    }
}

/// Number of hits a search field returns: `default` without `first`, at most `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchLimits {
    pub default: usize,
    pub max: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            default: 10,
            max: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchIndexMetadata {
    pub table_schema: String,
    pub table_name: String,
    pub index_name: String,
    pub column_name: String,
    pub access_method: String,
    pub operator_class: String,
    pub kind: SearchIndexKind,
    /// Columns an expression index reads (for naming it in `<Type>FullTextField`).
    #[serde(default)]
    pub columns: Vec<String>,
}

/// One concrete type in a Single Table Inheritance (STI) hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StiVariant {
    /// DB discriminator column value, e.g. "dog"
    pub discriminator_value: String,
    /// GraphQL type name for this variant, e.g. "Dog"
    pub type_name: String,
}

/// Single Table Inheritance detected from a discriminator column + CHECK or ENUM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StiMetadata {
    pub schema: String,
    pub table_name: String,
    /// Column holding the discriminator, e.g. "type" or "kind"
    pub discriminator_column: String,
    /// Ordered variants derived from CHECK IN or pg_enum values
    pub variants: Vec<StiVariant>,
    /// Concrete type for rows whose discriminator matches no variant (only when the
    /// discriminator is nullable), e.g. "GenericAnimal".
    #[serde(default)]
    pub fallback_type_name: Option<String>,
    /// GraphQL interface name: the base table's type name.
    #[serde(default)]
    pub base_type_name: String,
}

/// How a table-per-type hierarchy was detected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CtiKind {
    /// PostgreSQL native `INHERITS` (`pg_inherits`, partitions excluded).
    Inherits,
    /// Subtype tables whose primary key is also a foreign key to the parent's primary key.
    SharedPrimaryKey,
}

/// One child (subtype) table in a table-per-type hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CtiChild {
    pub schema: String,
    pub table: String,
    /// GraphQL type name for this child, e.g. "Dog"
    pub type_name: String,
    /// `(child_column, parent_column)` pairs joining a parent row to its child row.
    pub join_columns: Vec<(String, String)>,
}

/// Table-per-type inheritance: a parent table whose rows are specialised by child tables,
/// detected from `pg_inherits` or from shared-primary-key foreign keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CtiMetadata {
    pub schema: String,
    pub parent_table: String,
    pub kind: CtiKind,
    /// Concrete type for parent rows without a child row, e.g. "GenericVehicle".
    #[serde(default)]
    pub fallback_type_name: String,
    /// Most specific first: when several children match a row, the first one wins.
    pub children: Vec<CtiChild>,
    /// GraphQL interface name: the parent table's type name.
    #[serde(default)]
    pub base_type_name: String,
}

/// A table whose rows resolve to one of several concrete GraphQL types. The table's own
/// type becomes a GraphQL interface implemented by every concrete type.
#[derive(Debug, Clone, Copy)]
pub enum Polymorphism<'a> {
    Sti(&'a StiMetadata),
    Cti(&'a CtiMetadata),
}

impl<'a> Polymorphism<'a> {
    /// GraphQL interface name: the table's own type name, e.g. "Animal".
    pub fn interface_name(&self) -> String {
        let name = match self {
            Self::Sti(s) => &s.base_type_name,
            Self::Cti(c) => &c.base_type_name,
        };
        if name.is_empty() {
            table_to_type_name(self.table())
        } else {
            name.clone()
        }
    }

    pub fn fallback_type_name(&self) -> Option<&'a str> {
        match self {
            Self::Sti(s) => s.fallback_type_name.as_deref(),
            Self::Cti(c) => Some(c.fallback_type_name.as_str()),
        }
    }

    pub fn schema(&self) -> &'a str {
        match self {
            Self::Sti(s) => &s.schema,
            Self::Cti(c) => &c.schema,
        }
    }

    pub fn table(&self) -> &'a str {
        match self {
            Self::Sti(s) => &s.table_name,
            Self::Cti(c) => &c.parent_table,
        }
    }

    /// Concrete types implementing the interface; the fallback type (if any) is last.
    pub fn member_type_names(&self) -> Vec<String> {
        let mut names: Vec<String> = match self {
            Self::Sti(s) => s.variants.iter().map(|v| v.type_name.clone()).collect(),
            Self::Cti(c) => c.children.iter().map(|ch| ch.type_name.clone()).collect(),
        };
        names.extend(self.fallback_type_name().map(str::to_string));
        names
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatabaseCatalog {
    pub tables: Vec<TableMetadata>,
    pub functions: Vec<FunctionMetadata>,
    pub exclusive_arcs: Vec<ExclusiveArcMetadata>,
    pub many_to_many: Vec<ManyToManyMetadata>,
    pub search_indexes: Vec<SearchIndexMetadata>,
    #[serde(default)]
    pub sti: Vec<StiMetadata>,
    #[serde(default)]
    pub cti: Vec<CtiMetadata>,
    #[serde(default)]
    pub enums: Vec<EnumTypeMetadata>,
    #[serde(default)]
    pub composites: Vec<CompositeTypeMetadata>,
    /// Names of the installed extensions.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Domains exposed as scalars.
    #[serde(default)]
    pub domains: Vec<DomainMetadata>,
    /// Hits per search field (`search` in the config).
    #[serde(default)]
    pub search_limits: SearchLimits,
    /// `server_version_num` of the inspected server (e.g. `180006`); features of newer
    /// servers (`RETURNING OLD`, temporal keys) are used only when it has them.
    #[serde(default)]
    pub server_version: u32,
    /// Apollo Federation subgraph mode enabled.
    #[serde(default)]
    pub federation_enabled: bool,
    /// Federation version (e.g. "v2.3" or "v1").
    #[serde(default)]
    pub federation_version: Option<String>,
    /// Lookup maps derived from the catalog, built on first use. A catalog is immutable once
    /// inspection finishes; a schema reload builds a new catalog (and so a new index).
    #[serde(skip)]
    index: OnceLock<CatalogIndex>,
}

/// Mutation performed by a root mutation field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationAction {
    /// One row (`createUser`).
    Create,
    /// One row by primary or unique key (`updateUser`, `updateUserByEmail`).
    Update,
    Delete,
    /// Several rows (`createUsers(inputs: ...)`).
    CreateMany,
    /// The rows matching a filter (`updateUsers(where: ..., set: ...)`).
    UpdateMany,
    DeleteMany,
}

impl MutationAction {
    pub const ALL: [MutationAction; 6] = [
        Self::Create,
        Self::Update,
        Self::Delete,
        Self::CreateMany,
        Self::UpdateMany,
        Self::DeleteMany,
    ];

    /// `CreateUserInput` / `CreateUserPayload` prefix.
    pub fn type_prefix(self) -> &'static str {
        match self {
            Self::Create | Self::CreateMany => "Create",
            Self::Update | Self::UpdateMany => "Update",
            Self::Delete | Self::DeleteMany => "Delete",
        }
    }

    /// Affects any number of rows: the payload holds a list and `affectedCount`.
    pub fn is_many(self) -> bool {
        matches!(self, Self::CreateMany | Self::UpdateMany | Self::DeleteMany)
    }

    pub fn is_create(self) -> bool {
        matches!(self, Self::Create | Self::CreateMany)
    }

    /// `createUser`, `createUsers`, `updateUserByEmail`, ... (`key_suffix`: `ByEmail`).
    pub fn field_name(self, table: &TableMetadata, key_suffix: &str) -> String {
        let verb = self.type_prefix().to_ascii_lowercase();
        format!("{verb}{}{key_suffix}", self.subject(table))
    }

    /// `CreateUserPayload`, `CreateUsersPayload`.
    pub fn payload_type_name(self, table: &TableMetadata) -> String {
        format!("{}{}Payload", self.type_prefix(), self.subject(table))
    }

    /// Payload field holding the affected row(s): `user`, or `users` for many rows.
    pub fn payload_field_name(self, table: &TableMetadata) -> String {
        to_camel_case(&self.subject(table))
    }

    fn subject(self, table: &TableMetadata) -> String {
        if self.is_many() {
            table.plural_type_name()
        } else {
            table.type_name.clone()
        }
    }
}

/// A root mutation field and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationField {
    pub name: String,
    pub table: String,
    pub action: MutationAction,
    /// Columns of the unique key addressing the row of an update or delete by unique key
    /// (`updateUserByEmail`); empty for the primary key.
    pub unique_key: Vec<String>,
}

/// A field of a table's GraphQL object type and what backs it. Computed once per catalog and
/// shared by the schema builder (which emits it) and the planner (which resolves selections
/// through it), so field names cannot drift apart between the two.
#[derive(Debug, Clone)]
pub struct ObjectField {
    pub name: String,
    pub kind: FieldKind,
}

#[derive(Debug, Clone)]
pub enum FieldKind {
    /// Relay global object identifier.
    GlobalId,
    /// Raw single-column primary key.
    DatabaseId,
    /// A column, by name.
    Column(String),
    /// Related row (`!is_many`) or list of related rows.
    Relation(RelationInfo),
    /// Relay connection over a to-many relation.
    Connection(RelationInfo),
    /// The exclusive-arc union of the table.
    ArcTarget,
    /// Function on the row (index into `DatabaseCatalog::functions`): a computed column
    /// taking the row, or a function mounted with arguments from its columns.
    Function(usize),
    /// Version of the row for optimistic concurrency: its `xmin` (the transaction that last
    /// wrote it), or the configured version column.
    RowVersion,
    /// The rows above (`ancestors`) or below (`descendants`) the row in a hierarchy.
    Tree(TreeInfo),
}

/// A hierarchy of the rows of one table, declared by the database: a foreign key
/// referencing its own table, or an `ltree` path column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeInfo {
    /// Rows above the row (its parent, the parent's parent, ...), else rows below it.
    pub ancestors: bool,
    pub link: TreeLink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeLink {
    /// `(column, referenced column)` pairs of a foreign key referencing the table itself.
    ForeignKey(Vec<(String, String)>),
    /// An `ltree` column: a row is below the rows whose path is a prefix of its own.
    Path(String),
}

/// What an `<Type>OrderBy` value sorts rows by.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    Column(String),
    /// A column of the row a to-one relation leads to (`AUTHOR_NAME`).
    Related {
        rel: RelationInfo,
        column: String,
    },
    /// The number of rows of a to-many relation (`MESSAGES_COUNT`).
    Count(RelationInfo),
    /// A computed column's value (index into `DatabaseCatalog::functions`).
    Function(usize),
}

/// Root field searching every searchable table.
const GLOBAL_SEARCH_FIELD: &str = "globalSearch";

/// Name → table maps the planner needs on every request, derived once per catalog.
#[derive(Debug, Clone, Default)]
struct CatalogIndex {
    /// schema → root query field → (table, is_single, is_connection).
    /// The first table claiming a name wins, matching schema-builder order.
    root_fields: HashMap<String, HashMap<String, (String, bool, bool)>>,
    /// schema → search root field → (table, method). Names taken by other root fields
    /// are skipped.
    search_fields: HashMap<String, HashMap<String, (String, SearchMethod)>>,
    /// schema → root query field → function (index into `functions`), and the same for
    /// mutation fields (`VOLATILE` functions). Names taken by other root fields are
    /// skipped; of overloaded functions the first (by OID) wins.
    function_queries: HashMap<String, HashMap<String, usize>>,
    function_mutations: HashMap<String, HashMap<String, usize>>,
    /// schema → GraphQL type name → table.
    type_tables: HashMap<String, HashMap<String, String>>,
    /// schema → mutation field name → field.
    mutation_fields: HashMap<String, HashMap<String, MutationField>>,
    /// (schema, table) → fields of the table's object type, in schema order.
    object_fields: HashMap<(String, String), Vec<ObjectField>>,
    /// Schemas with a `globalSearch` root field, and the types it finds.
    global_search_schemas: HashSet<String>,
    global_search_types: HashSet<String>,
}

impl CatalogIndex {
    fn build(catalog: &DatabaseCatalog) -> Self {
        let mut index = Self::default();
        for t in &catalog.tables {
            let ops = t.operations;
            let roots = index.root_fields.entry(t.schema.clone()).or_default();
            let mut names: Vec<(String, bool, bool)> = Vec::new();
            if ops.list {
                names.push((t.list_field.clone(), false, false));
            }
            if ops.read {
                names.push((t.single_field.clone(), true, false));
            }
            if ops.connection {
                names.push((t.connection_field.clone(), false, true));
                names.extend(t.all_connection_field.clone().map(|n| (n, false, true)));
            }
            names.extend(
                catalog
                    .unique_lookup_fields(t)
                    .into_iter()
                    .map(|(n, _)| (n, true, false)),
            );
            for (name, single, connection) in names {
                roots
                    .entry(name)
                    .or_insert_with(|| (t.name.clone(), single, connection));
            }
            index
                .type_tables
                .entry(t.schema.clone())
                .or_default()
                .entry(t.type_name.clone())
                .or_insert_with(|| t.name.clone());
            if let Some(Polymorphism::Sti(sti)) = catalog.find_polymorphism(&t.schema, &t.name) {
                for v in &sti.variants {
                    index
                        .type_tables
                        .entry(t.schema.clone())
                        .or_default()
                        .entry(v.type_name.clone())
                        .or_insert_with(|| t.name.clone());
                }
            }
            let mutations = index.mutation_fields.entry(t.schema.clone()).or_default();
            for field in catalog.table_mutation_fields(t) {
                mutations.entry(field.name.clone()).or_insert(field);
            }
            index.object_fields.insert(
                (t.schema.clone(), t.name.clone()),
                catalog.build_object_fields(t),
            );
        }
        for t in &catalog.tables {
            let roots = index.root_fields.get(&t.schema);
            let searches = index.search_fields.entry(t.schema.clone()).or_default();
            for method in catalog.search_methods(t) {
                let name = format!("{}{}", t.list_field, method.field_suffix());
                if !roots.is_some_and(|r| r.contains_key(&name)) {
                    searches
                        .entry(name)
                        .or_insert_with(|| (t.name.clone(), method));
                }
            }
        }
        let global = catalog.global_search_tables();
        for (t, _) in &global {
            let taken = index
                .root_fields
                .get(&t.schema)
                .is_some_and(|r| r.contains_key(GLOBAL_SEARCH_FIELD))
                || index
                    .search_fields
                    .get(&t.schema)
                    .is_some_and(|r| r.contains_key(GLOBAL_SEARCH_FIELD));
            if !taken {
                index.global_search_schemas.insert(t.schema.clone());
            }
        }
        index.global_search_types = catalog.global_search_types().into_iter().collect();
        for (i, f) in catalog.functions.iter().enumerate() {
            if f.omit_root || !catalog.function_is_exposed(f) {
                continue;
            }
            let name = f.field_name.clone();
            if f.volatile {
                let taken = index
                    .mutation_fields
                    .get(&f.schema)
                    .is_some_and(|m| m.contains_key(&name));
                if !taken {
                    index
                        .function_mutations
                        .entry(f.schema.clone())
                        .or_default()
                        .entry(name)
                        .or_insert(i);
                }
            } else if f.returns != FunctionReturn::Void {
                let taken = index
                    .root_fields
                    .get(&f.schema)
                    .is_some_and(|r| r.contains_key(&name))
                    || index
                        .search_fields
                        .get(&f.schema)
                        .is_some_and(|r| r.contains_key(&name))
                    || (name == GLOBAL_SEARCH_FIELD
                        && index.global_search_schemas.contains(&f.schema));
                if !taken {
                    index
                        .function_queries
                        .entry(f.schema.clone())
                        .or_default()
                        .entry(name)
                        .or_insert(i);
                }
            }
        }
        index
    }
}

/// A relation followed from a row of a source table to rows of a target table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JunctionRelationInfo {
    pub junction_schema: String,
    pub junction_table: String,
    /// `(junction column, source column)` pairs.
    pub source_columns: Vec<(String, String)>,
    /// `(junction column, target column)` pairs.
    pub target_columns: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelationInfo {
    /// True if 1:N or M:N (list of records), false if N:1 (single record)
    pub is_many: bool,
    pub target_schema: String,
    pub target_table: String,
    /// `(target column, source column)` pairs joining a target row to the source row; for
    /// many-to-many relations the source side is the junction (see `junction`).
    pub columns: Vec<(String, String)>,
    pub junction: Option<JunctionRelationInfo>,
    /// `(target column, source column)` ranges of a temporal foreign key: related rows are
    /// those whose range overlaps the source row's.
    #[serde(default)]
    pub period: Option<(String, String)>,
    /// The other versions of a row of a table with a temporal key: the rows sharing its key
    /// apart from the period, in period order.
    #[serde(default)]
    pub history: bool,
}

impl DatabaseCatalog {
    fn index(&self) -> &CatalogIndex {
        self.index.get_or_init(|| CatalogIndex::build(self))
    }

    /// A copy with only the tables and functions of `schema`: relations to other schemas are
    /// gone from it.
    pub fn only_schema(&self, schema: &str) -> Self {
        let mut copy = self.clone();
        copy.tables.retain(|t| t.schema == schema);
        copy.functions.retain(|f| f.schema == schema);
        copy.invalidate_index();
        copy
    }

    /// Drops derived lookup maps after the catalog changed.
    pub fn invalidate_index(&mut self) {
        self.index = OnceLock::new();
    }

    /// Table behind a root query field: `(table, is_single, is_connection)`.
    pub fn resolve_root_field(&self, schema: &str, field_name: &str) -> Option<(&str, bool, bool)> {
        let (table, single, connection) = self.index().root_fields.get(schema)?.get(field_name)?;
        Some((table.as_str(), *single, *connection))
    }

    /// Table behind a root query field searching across all schemas: `(schema, table, is_single, is_connection)`.
    pub fn resolve_root_field_any(&self, field_name: &str) -> Option<(&str, &str, bool, bool)> {
        for (schema, roots) in &self.index().root_fields {
            if let Some((table, single, conn)) = roots.get(field_name) {
                return Some((schema.as_str(), table.as_str(), *single, *conn));
            }
        }
        None
    }

    /// Function behind a root query field.
    pub fn resolve_function_query(
        &self,
        schema: &str,
        field_name: &str,
    ) -> Option<(usize, &FunctionMetadata)> {
        let i = *self.index().function_queries.get(schema)?.get(field_name)?;
        Some((i, &self.functions[i]))
    }

    /// Function behind a root query field in any schema.
    pub fn resolve_function_query_any(
        &self,
        field_name: &str,
    ) -> Option<(usize, &FunctionMetadata)> {
        for funcs in self.index().function_queries.values() {
            if let Some(&i) = funcs.get(field_name) {
                return Some((i, &self.functions[i]));
            }
        }
        None
    }

    /// Function behind a root mutation field.
    pub fn resolve_function_mutation(
        &self,
        schema: &str,
        field_name: &str,
    ) -> Option<(usize, &FunctionMetadata)> {
        let i = *self
            .index()
            .function_mutations
            .get(schema)?
            .get(field_name)?;
        Some((i, &self.functions[i]))
    }

    /// Function behind a root mutation field in any schema.
    pub fn resolve_function_mutation_any(
        &self,
        field_name: &str,
    ) -> Option<(usize, &FunctionMetadata)> {
        for funcs in self.index().function_mutations.values() {
            if let Some(&i) = funcs.get(field_name) {
                return Some((i, &self.functions[i]));
            }
        }
        None
    }

    /// Root query (`mutation: false`) or mutation fields backed by functions, in catalog order.
    pub fn function_root_fields(&self, mutation: bool) -> Vec<&FunctionMetadata> {
        self.functions
            .iter()
            .enumerate()
            .filter(|(i, f)| {
                let resolved = if mutation {
                    self.resolve_function_mutation(&f.schema, &f.field_name)
                } else {
                    self.resolve_function_query(&f.schema, &f.field_name)
                };
                resolved.is_some_and(|(r, _)| r == *i)
            })
            .map(|(_, f)| f)
            .collect()
    }

    /// Whether a function's result has a GraphQL type: rows of an exposed table, a value
    /// type the schema has, its own columns, or nothing (`void`).
    pub fn function_is_exposed(&self, f: &FunctionMetadata) -> bool {
        match &f.returns {
            FunctionReturn::Table { schema, table } => self.find_table(schema, table).is_some(),
            FunctionReturn::Record => !f.return_columns.is_empty(),
            FunctionReturn::Value(_) | FunctionReturn::Void => true,
        }
    }

    /// Table and method behind a search root field.
    pub fn resolve_search_field(
        &self,
        schema: &str,
        field_name: &str,
    ) -> Option<(&str, SearchMethod)> {
        let (table, method) = self.index().search_fields.get(schema)?.get(field_name)?;
        Some((table.as_str(), *method))
    }

    /// Table and method behind a search root field in any schema: `(schema, table, method)`.
    pub fn resolve_search_field_any(&self, field_name: &str) -> Option<(&str, &str, SearchMethod)> {
        self.index()
            .search_fields
            .iter()
            .find_map(|(schema, fields)| {
                fields
                    .get(field_name)
                    .map(|(table, method)| (schema.as_str(), table.as_str(), *method))
            })
    }

    /// Search root fields of `table`, in `SearchMethod::ALL` order.
    pub fn search_fields(&self, table: &TableMetadata) -> Vec<(String, SearchMethod)> {
        self.search_methods(table)
            .into_iter()
            .map(|method| {
                (
                    format!("{}{}", table.list_field, method.field_suffix()),
                    method,
                )
            })
            .filter(|(name, method)| {
                self.resolve_search_field(&table.schema, name)
                    == Some((table.name.as_str(), *method))
            })
            .collect()
    }

    /// Search methods the indexes of `table` support. Searching reads rows, so it needs
    /// the list or connection operation.
    pub fn search_methods(&self, table: &TableMetadata) -> Vec<SearchMethod> {
        if !(table.operations.list || table.operations.connection) {
            return Vec::new();
        }
        let (schema, name) = (&table.schema, &table.name);
        SearchMethod::ALL
            .into_iter()
            .filter(|method| match method {
                SearchMethod::Fuzzy => !self.fuzzy_indexes_for_table(schema, name).is_empty(),
                SearchMethod::Semantic => !self.vector_indexes_for_table(schema, name).is_empty(),
                SearchMethod::Bm25 => !self.bm25_indexes_for_table(schema, name).is_empty(),
                SearchMethod::Hybrid => self.has_hybrid_search(schema, name),
                SearchMethod::FullText => {
                    !self.full_text_indexes_for_table(schema, name).is_empty()
                }
                SearchMethod::Nearest => !self.spatial_indexes_for_table(schema, name).is_empty(),
            })
            .collect()
    }

    /// The root mutation field `field_name`.
    pub fn resolve_mutation_field(&self, schema: &str, field_name: &str) -> Option<&MutationField> {
        self.index().mutation_fields.get(schema)?.get(field_name)
    }

    /// The root mutation field `field_name` across any schema: `(schema, field)`.
    pub fn resolve_mutation_field_any(&self, field_name: &str) -> Option<(&str, &MutationField)> {
        for (schema, fields) in &self.index().mutation_fields {
            if let Some(field) = fields.get(field_name) {
                return Some((schema.as_str(), field));
            }
        }
        None
    }

    /// Root query fields looking a row up by a unique key other than the primary key
    /// (`userByEmail`), with the key's columns.
    pub fn unique_lookup_fields<'t>(
        &self,
        table: &'t TableMetadata,
    ) -> Vec<(String, Vec<&'t ColumnMetadata>)> {
        if !table.operations.read {
            return Vec::new();
        }
        table
            .lookup_keys()
            .map(|(_, cols)| {
                (
                    format!("{}{}", table.single_field, unique_key_suffix(&cols)),
                    cols,
                )
            })
            .collect()
    }

    /// Relations a `Create<Type>Input` of `table` nests, by field name: to-one relations
    /// (`author: { create | connect }`), to-many relations and many-to-many relations
    /// (`messages: { create: [...], connect: [...] }`). A relation reachable under several
    /// names nests under the first; one whose target can be neither created nor connected, or
    /// whose name is taken by a column field, is left out.
    pub fn nested_relations(&self, table: &TableMetadata) -> Vec<(String, RelationInfo)> {
        let mut out: Vec<(String, RelationInfo)> = Vec::new();
        for field in self.object_fields(&table.schema, &table.name) {
            let FieldKind::Relation(rel) = &field.kind else {
                continue;
            };
            if out.iter().any(|(_, r)| r == rel)
                || table.writable_columns().any(|c| c.field_name == field.name)
            {
                continue;
            }
            let Some(target) = self.find_table(&rel.target_schema, &rel.target_table) else {
                continue;
            };
            let usable = match &rel.junction {
                Some(j) => {
                    self.find_table(&j.junction_schema, &j.junction_table)
                        .is_some_and(|t| t.operations.create)
                        && (target.is_insertable() || target.is_connectable())
                }
                // Connecting existing children updates their foreign key
                None if rel.is_many => {
                    target.is_insertable()
                        || (target.operations.update && !target.primary_key.is_empty())
                }
                None => target.is_insertable() || target.is_connectable(),
            };
            if usable {
                out.push((field.name.clone(), rel.clone()));
            }
        }
        out
    }

    /// Root mutation fields of `table`, in schema order: per verb the single-row field, its
    /// by-unique-key variants, then the many-rows field.
    pub fn table_mutation_fields(&self, table: &TableMetadata) -> Vec<MutationField> {
        let ops = table.operations;
        let writable = table.writable_columns().next().is_some();
        let has_pk = !table.primary_key.is_empty();
        let lookups: Vec<Vec<&ColumnMetadata>> =
            table.lookup_keys().map(|(_, cols)| cols).collect();
        let mut out = Vec::new();
        let mut push = |action: MutationAction, key: &[&ColumnMetadata]| {
            out.push(MutationField {
                name: action.field_name(
                    table,
                    &if key.is_empty() {
                        String::new()
                    } else {
                        unique_key_suffix(key)
                    },
                ),
                table: table.name.clone(),
                action,
                unique_key: key.iter().map(|c| c.name.clone()).collect(),
            });
        };
        if ops.create {
            push(MutationAction::Create, &[]);
        }
        if ops.create_many && writable {
            push(MutationAction::CreateMany, &[]);
        }
        if ops.update && writable {
            if has_pk {
                push(MutationAction::Update, &[]);
            }
            for key in &lookups {
                push(MutationAction::Update, key);
            }
        }
        if ops.update_many && writable {
            push(MutationAction::UpdateMany, &[]);
        }
        if ops.delete {
            if has_pk {
                push(MutationAction::Delete, &[]);
            }
            for key in &lookups {
                push(MutationAction::Delete, key);
            }
        }
        if ops.delete_many {
            push(MutationAction::DeleteMany, &[]);
        }
        out
    }

    /// Table whose GraphQL type is `type_name` (e.g. `OrderItem` -> `order_items`).
    pub fn table_for_type(&self, schema: &str, type_name: &str) -> Option<&str> {
        self.index()
            .type_tables
            .get(schema)?
            .get(type_name)
            .map(String::as_str)
    }

    /// Table whose GraphQL type is `type_name` across any schema: `(schema, table)`.
    pub fn table_for_type_any(&self, type_name: &str) -> Option<(&str, &str)> {
        for (schema, types) in &self.index().type_tables {
            if let Some(table) = types.get(type_name) {
                return Some((schema.as_str(), table.as_str()));
            }
        }
        None
    }

    /// Fields of the object type of `(schema, table)`, in schema order.
    pub fn object_fields(&self, schema: &str, table: &str) -> &[ObjectField] {
        self.index()
            .object_fields
            .get(&(schema.to_string(), table.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Field `name` of the object type of `(schema, table)`.
    pub fn object_field(&self, schema: &str, table: &str, name: &str) -> Option<&ObjectField> {
        self.object_fields(schema, table)
            .iter()
            .find(|f| f.name == name)
    }

    pub fn find_enum(&self, key: &str) -> Option<&EnumTypeMetadata> {
        self.enums.iter().find(|e| e.key == key)
    }

    pub fn find_composite(&self, key: &str) -> Option<&CompositeTypeMetadata> {
        self.composites.iter().find(|c| c.key == key)
    }

    /// GraphQL type of a (nullable) value of `pg_type`: enums and composites by name (the
    /// composite's input type when `input`), lists for arrays. `enum_key` names the enum of a
    /// `CHECK`-closed text column.
    pub fn graphql_type(&self, pg_type: &PgType, enum_key: Option<&str>, input: bool) -> String {
        if let PgType::Array(elem) = pg_type {
            return format!("[{}]", self.graphql_type(elem, enum_key, input));
        }
        let enum_key = enum_key.or(match pg_type {
            PgType::Enum(key) => Some(key.as_str()),
            _ => None,
        });
        if let Some(e) = enum_key.and_then(|k| self.find_enum(k)) {
            return e.type_name.clone();
        }
        match pg_type {
            PgType::Composite(key) => match self.find_composite(key) {
                Some(c) if input => c.input_type_name(),
                Some(c) => c.type_name.clone(),
                None => "JSON".to_string(),
            },
            range if input && range.is_range() => format!("{}Input", range.graphql_type()),
            PgType::Multirange(range) if input => format!("[{}Input!]", range.graphql_type()),
            other => other.graphql_type(),
        }
    }

    /// GraphQL type of a column's value: its domain's scalar, or its type's.
    pub fn column_type(&self, col: &ColumnMetadata, input: bool) -> String {
        if let Some(d) = col.domain.as_deref().and_then(|k| self.find_domain(k)) {
            return d.type_name.clone();
        }
        self.graphql_type(&col.pg_type, col.enum_key.as_deref(), input)
    }

    pub fn find_domain(&self, key: &str) -> Option<&DomainMetadata> {
        self.domains.iter().find(|d| d.key == key)
    }

    /// Enum of a column (or of its elements).
    pub fn column_enum(&self, col: &ColumnMetadata) -> Option<&EnumTypeMetadata> {
        self.find_enum(col.enum_key.as_deref()?)
    }

    /// GraphQL type name of a table (its own name, derived, if unknown).
    pub fn type_name_of(&self, schema: &str, table: &str) -> String {
        match self.find_table(schema, table) {
            Some(t) => t.type_name.clone(),
            None => table_to_type_name(table),
        }
    }

    pub fn find_table(&self, schema: &str, name: &str) -> Option<&TableMetadata> {
        self.tables
            .iter()
            .find(|t| t.schema == schema && t.name == name)
    }

    /// Payload field holding the rows as they were before a write (`previousUser`,
    /// `previousUsers`), read with `RETURNING OLD` (PostgreSQL 18): updates, and creates that
    /// can upsert. `None` otherwise.
    pub fn previous_field_name(
        &self,
        table: &TableMetadata,
        action: MutationAction,
        upsert: bool,
    ) -> Option<String> {
        let applies = match action {
            MutationAction::Update | MutationAction::UpdateMany => true,
            MutationAction::Create | MutationAction::CreateMany => upsert,
            _ => false,
        };
        (applies && self.server_version >= 180000).then(|| {
            format!(
                "previous{}",
                to_pascal_case(&action.payload_field_name(table))
            )
        })
    }

    /// Constraint `name` of `(schema, table)`.
    pub fn find_constraint(
        &self,
        schema: &str,
        table: &str,
        name: &str,
    ) -> Option<(&TableMetadata, &ConstraintMetadata)> {
        let t = self.find_table(schema, table)?;
        Some((t, t.constraints.iter().find(|c| c.name == name)?))
    }

    /// Error code of a column-level violation: `<FIELD>_REQUIRED` (NOT NULL),
    /// `<FIELD>_TOO_LONG` (`varchar(n)`), `<FIELD>_OUT_OF_RANGE` (`numeric(p, s)`).
    pub fn column_error_code(col: &ColumnMetadata, suffix: &str) -> String {
        format!("{}_{suffix}", col.order_by_name())
    }

    /// Values of `table`'s `<Type>ErrorCode` enum, `(code, description)`: violations of its
    /// columns' limits and NOT NULL, of its constraints, and of foreign keys referencing it.
    pub fn error_codes(&self, table: &TableMetadata) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push = |code: String, doc: String| {
            if !code.is_empty() && !out.iter().any(|(c, _)| *c == code) {
                out.push((code, doc));
            }
        };
        for col in table.writable_columns() {
            let field = &col.field_name;
            if !col.is_nullable {
                push(
                    Self::column_error_code(col, "REQUIRED"),
                    format!("`{field}` is required (it cannot be null)."),
                );
            }
            if let Some(n) = col.limits.max_length {
                push(
                    Self::column_error_code(col, "TOO_LONG"),
                    format!("`{field}` is longer than {n} characters."),
                );
            }
            if let Some((p, s)) = col.limits.precision {
                push(
                    Self::column_error_code(col, "OUT_OF_RANGE"),
                    format!("`{field}` does not fit `numeric({p}, {s})`."),
                );
            }
        }
        if table.has_row_version() {
            push(
                "STALE_VERSION".to_string(),
                "`expectedVersion` is not the row's `rowVersion`: the row changed since that version was read.".to_string(),
            );
        }
        let fields = |t: &TableMetadata, columns: &[String]| -> String {
            columns
                .iter()
                .map(|c| {
                    format!(
                        "`{}`",
                        t.get_column(c).map(|c| c.field_name.as_str()).unwrap_or(c)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        for con in &table.constraints {
            let doc = match con.kind {
                ConstraintKind::PrimaryKey => format!(
                    "A row with this primary key ({}) already exists.",
                    fields(table, &con.columns)
                ),
                ConstraintKind::Unique => format!(
                    "Another row already has this {} (unique `{}`).",
                    fields(table, &con.columns),
                    con.name
                ),
                ConstraintKind::ForeignKey => {
                    let target = con
                        .references
                        .as_ref()
                        .map(|(s, t)| self.type_name_of(s, t))
                        .unwrap_or_default();
                    format!(
                        "{} refers to a `{target}` that does not exist (`{}`).",
                        fields(table, &con.columns),
                        con.name
                    )
                }
                ConstraintKind::Check => format!(
                    "Check `{}` failed: `{}`.",
                    con.name,
                    con.check_expression()
                        .unwrap_or_else(|| con.definition.clone())
                        .replace('`', "'")
                ),
                ConstraintKind::Exclusion => {
                    format!(
                        "Conflicts with another row on {} (exclusion `{}`).",
                        fields(table, &con.columns),
                        con.name
                    )
                }
            };
            push(con.code.clone(), doc);
        }
        for other in &self.tables {
            for con in other
                .constraints
                .iter()
                .filter(|c| c.kind == ConstraintKind::ForeignKey)
            {
                if con
                    .references
                    .as_ref()
                    .is_some_and(|(s, t)| *s == table.schema && *t == table.name)
                {
                    push(
                        con.referenced_code.clone(),
                        format!(
                            "`{}` rows still refer to it (`{}`).",
                            other.type_name, con.name
                        ),
                    );
                }
            }
        }
        out
    }

    /// Assigns the error code of every constraint (see [`ConstraintMetadata::code`]),
    /// unique within each table's `<Type>ErrorCode` enum. Runs once names are final.
    pub fn assign_error_codes(&mut self) {
        // Column-level codes first: they are derived again wherever they are reported
        let mut taken: HashMap<(String, String), HashSet<String>> = HashMap::new();
        for t in &self.tables {
            let set = taken.entry((t.schema.clone(), t.name.clone())).or_default();
            set.insert("STALE_VERSION".to_string());
            for col in &t.columns {
                for suffix in ["REQUIRED", "TOO_LONG", "OUT_OF_RANGE"] {
                    set.insert(Self::column_error_code(col, suffix));
                }
            }
        }
        let screaming = |columns: &[String], t: &TableMetadata| -> String {
            let names: Vec<String> = columns
                .iter()
                .map(|c| {
                    t.get_column(c)
                        .map(|c| c.order_by_name())
                        .unwrap_or_else(|| enum_value_name(c))
                })
                .collect();
            if names.is_empty() {
                "ROW".to_string()
            } else {
                names.join("_AND_")
            }
        };
        // Relation fields naming foreign keys: the to-one field, and the to-many field on
        // the referenced type
        let relation_field = |t: &TableMetadata,
                              con: &ConstraintMetadata,
                              many: bool|
         -> Option<String> {
            let (schema, table) = con.references.as_ref()?;
            let pairs: Vec<(String, String)> = con
                .columns
                .iter()
                .cloned()
                .zip(self.fk_foreign_columns(t, con))
                .collect();
            let (owner_schema, owner) = if many {
                (schema.as_str(), table.as_str())
            } else {
                (t.schema.as_str(), t.name.as_str())
            };
            self.object_fields(owner_schema, owner)
                .iter()
                .find_map(|f| match &f.kind {
                    FieldKind::Relation(rel) if rel.is_many == many && rel.junction.is_none() => {
                        let matches = if many {
                            rel.target_schema == t.schema
                                && rel.target_table == t.name
                                && rel.columns == pairs
                        } else {
                            rel.target_schema == *schema
                                && rel.target_table == *table
                                && rel
                                    .columns
                                    .iter()
                                    .map(|(a, b)| (b.clone(), a.clone()))
                                    .collect::<Vec<_>>()
                                    == pairs
                        };
                        matches.then(|| f.name.clone())
                    }
                    _ => None,
                })
        };

        let mut codes: Vec<(usize, usize, String, String)> = Vec::new();
        for (ti, t) in self.tables.iter().enumerate() {
            for (ci, con) in t.constraints.iter().enumerate() {
                let base = match con.kind {
                    ConstraintKind::PrimaryKey | ConstraintKind::Unique => {
                        format!("{}_TAKEN", screaming(&con.columns, t))
                    }
                    ConstraintKind::ForeignKey => {
                        let name = relation_field(t, con, false)
                            .map(|f| enum_value_name(&f))
                            .or_else(|| {
                                con.references
                                    .as_ref()
                                    .map(|(s, tb)| enum_value_name(&self.type_name_of(s, tb)))
                            })
                            .unwrap_or_else(|| screaming(&con.columns, t));
                        format!("{name}_NOT_FOUND")
                    }
                    ConstraintKind::Check => {
                        match custom_constraint_name(&con.name, &t.name, "check") {
                            Some(custom) => enum_value_name(custom),
                            None => format!("{}_INVALID", screaming(&con.columns, t)),
                        }
                    }
                    ConstraintKind::Exclusion => {
                        match custom_constraint_name(&con.name, &t.name, "excl") {
                            Some(custom) => enum_value_name(custom),
                            None => format!("{}_CONFLICT", screaming(&con.columns, t)),
                        }
                    }
                };
                let referenced = match con.kind {
                    ConstraintKind::ForeignKey => {
                        let name =
                            relation_field(t, con, true).unwrap_or_else(|| t.plural_type_name());
                        format!("REFERENCED_BY_{}", enum_value_name(&name))
                    }
                    _ => String::new(),
                };
                codes.push((ti, ci, base, referenced));
            }
        }
        // Enum values cannot be `true`, `false` or `null`
        let valid = |code: String| {
            if matches!(code.as_str(), "TRUE" | "FALSE" | "NULL") {
                format!("{code}_VIOLATED")
            } else {
                code
            }
        };
        for (ti, ci, base, referenced) in codes {
            let base = valid(base);
            let key = (self.tables[ti].schema.clone(), self.tables[ti].name.clone());
            let code = claim_name_with(taken.entry(key).or_default(), &base, "_");
            let referenced_code = match self.tables[ti].constraints[ci].references.clone() {
                Some(target) if !referenced.is_empty() => {
                    claim_name_with(taken.entry(target).or_default(), &referenced, "_")
                }
                _ => String::new(),
            };
            let con = &mut self.tables[ti].constraints[ci];
            con.code = code;
            con.referenced_code = referenced_code;
        }
        self.invalidate_index();
    }

    /// Referenced columns of a foreign key constraint, from the table's foreign keys.
    fn fk_foreign_columns(&self, t: &TableMetadata, con: &ConstraintMetadata) -> Vec<String> {
        t.foreign_keys
            .iter()
            .find(|fk| fk.constraint_name == con.name)
            .map(|fk| fk.foreign_columns.clone())
            .unwrap_or_default()
    }

    /// Description of a column's field: its comment, then the limits of its type and the
    /// `CHECK` constraints on it alone (other than a closed value list, which is its enum).
    pub fn column_description(
        &self,
        table: &TableMetadata,
        col: &ColumnMetadata,
    ) -> Option<String> {
        let mut parts: Vec<String> = col
            .description
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        parts.extend(col.limits.describe());
        for con in table
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Check && c.columns == [col.name.clone()])
        {
            let is_enum = col.enum_key.as_deref()
                == Some(format!("check:{}.{}", table.name, col.name).as_str())
                && con
                    .check_expression()
                    .is_some_and(|e| e.contains(" = ANY (ARRAY[") || e.contains(" = '"));
            if let (false, Some(expr)) = (is_enum, con.check_expression()) {
                parts.push(format!("Must satisfy `{}`.", expr.replace('`', "'")));
            }
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Description of a table's type: its comment, then the `CHECK` constraints spanning
    /// several of its columns.
    pub fn table_description(&self, table: &TableMetadata) -> Option<String> {
        let mut parts: Vec<String> = table
            .description
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        let checks: Vec<String> = table
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Check && c.columns.len() > 1)
            .filter_map(|c| c.check_expression())
            .map(|e| format!("`{}`", e.replace('`', "'")))
            .collect();
        if !checks.is_empty() {
            parts.push(format!("Rows must satisfy {}.", checks.join(" and ")));
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// First table named `name` in any schema (for metadata that records only a table name).
    pub fn find_table_any(&self, name: &str) -> Option<&TableMetadata> {
        self.tables.iter().find(|t| t.name == name)
    }

    pub fn find_sti(&self, schema: &str, table: &str) -> Option<&StiMetadata> {
        self.sti
            .iter()
            .find(|s| s.schema == schema && s.table_name == table)
    }

    pub fn find_cti(&self, schema: &str, table: &str) -> Option<&CtiMetadata> {
        self.cti
            .iter()
            .find(|c| c.schema == schema && c.parent_table == table)
    }

    /// The columns only the subtype tables of a table-per-type hierarchy have, grouped by field
    /// name: a column of a subtype that the parent (or a more specific subtype's column of the
    /// same name and a different type) does not already claim. Each group lists the subtypes
    /// that have it, with the first one's type.
    pub fn cti_columns(
        &self,
        table: &TableMetadata,
    ) -> Vec<(String, Vec<(&CtiChild, &ColumnMetadata)>)> {
        let Some(cti) = self.find_cti(&table.schema, &table.name) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Vec<(&CtiChild, &ColumnMetadata)>)> = Vec::new();
        for child in &cti.children {
            let Some(child_table) = self.find_table(&child.schema, &child.table) else {
                continue;
            };
            for col in child_table.visible_columns() {
                if table
                    .visible_columns()
                    .any(|pc| pc.name == col.name || pc.field_name == col.field_name)
                {
                    continue;
                }
                match out.iter_mut().find(|(name, _)| *name == col.field_name) {
                    Some((_, members)) => {
                        if members[0].1.pg_type == col.pg_type {
                            members.push((child, col));
                        }
                    }
                    None => out.push((col.field_name.clone(), vec![(child, col)])),
                }
            }
        }
        out
    }

    /// Row polymorphism of a table. Table-per-type wins over a discriminator column, since
    /// child tables carry the subtype-specific columns.
    pub fn find_polymorphism(&self, schema: &str, table: &str) -> Option<Polymorphism<'_>> {
        self.find_cti(schema, table)
            .map(Polymorphism::Cti)
            .or_else(|| self.find_sti(schema, table).map(Polymorphism::Sti))
    }

    /// All polymorphic tables, CTI hierarchies first.
    pub fn polymorphisms(&self) -> impl Iterator<Item = Polymorphism<'_>> {
        self.cti
            .iter()
            .map(Polymorphism::Cti)
            .chain(self.sti.iter().map(Polymorphism::Sti))
    }

    /// CTI hierarchies `table` is a child of, in catalog order (the first parent provides
    /// fields shared by several parents, including the Relay `id`).
    pub fn cti_parents_of(&self, schema: &str, table: &str) -> Vec<(&CtiMetadata, &CtiChild)> {
        self.cti
            .iter()
            .filter_map(|c| {
                c.children
                    .iter()
                    .find(|ch| ch.schema == schema && ch.table == table)
                    .map(|ch| (c, ch))
            })
            .collect()
    }

    /// Interfaces implemented by the concrete GraphQL type `type_name`: the STI base of a
    /// variant/fallback, the CTI parents of a child table type or a CTI fallback.
    pub fn interfaces_of(&self, type_name: &str) -> Vec<String> {
        let mut out = Vec::new();
        for poly in self.polymorphisms() {
            if poly.member_type_names().iter().any(|m| m == type_name) {
                out.push(poly.interface_name());
            }
        }
        out
    }

    /// Concrete member types of an exclusive-arc union: a polymorphic target contributes its
    /// concrete types (GraphQL unions cannot contain interfaces).
    pub fn arc_member_types(
        &self,
        arc: &ExclusiveArcMetadata,
        default_schema: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (i, (_, schema, table, _)) in arc.branches(default_schema).enumerate() {
            let names = match self.find_polymorphism(schema, table) {
                Some(poly) => poly.member_type_names(),
                None => vec![arc.target_type_name(i)],
            };
            for name in names {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Default GraphQL names for everything not named yet: types and root fields from table
    /// names, fields from column names, function fields from function names.
    /// When table or function names collide across different database schemas, smart schema
    /// prefixing is applied to resolve the conflict without prefixing non-colliding names.
    pub fn assign_default_names(&mut self) {
        let mut table_name_counts: HashMap<String, usize> = HashMap::new();
        for t in &self.tables {
            *table_name_counts.entry(t.name.clone()).or_default() += 1;
        }

        for t in &mut self.tables {
            let conflict = table_name_counts.get(&t.name).copied().unwrap_or(0) > 1;
            let (prefix_pascal, prefix_camel) = if conflict {
                (to_pascal_case(&t.schema), to_camel_case(&t.schema))
            } else {
                (String::new(), String::new())
            };

            if t.type_name.is_empty() {
                let base = table_to_type_name(&t.name);
                t.type_name = if conflict {
                    format!("{prefix_pascal}{base}")
                } else {
                    base
                };
            }
            if t.list_field.is_empty() {
                let base = list_field_name(&t.name);
                t.list_field = if conflict {
                    format!("{prefix_camel}{}", to_pascal_case(&base))
                } else {
                    base
                };
            }
            if t.single_field.is_empty() {
                let base = single_field_name(&t.name);
                t.single_field = if conflict {
                    format!("{prefix_camel}{}", to_pascal_case(&base))
                } else {
                    base
                };
            }
            if t.connection_field.is_empty() {
                let (conn, all_conn) = connection_field_names(&t.name);
                t.connection_field = if conflict {
                    format!("{prefix_camel}{}", to_pascal_case(&conn))
                } else {
                    conn
                };
                t.all_connection_field = if all_conn != connection_field_names(&t.name).0 {
                    let field = if conflict {
                        format!("{prefix_camel}{}", to_pascal_case(&all_conn))
                    } else {
                        all_conn
                    };
                    Some(field)
                } else {
                    None
                };
            }
            for c in &mut t.columns {
                if c.field_name.is_empty() {
                    c.field_name = to_camel_case(&c.name);
                }
            }
        }
        for c in &mut self.composites {
            if c.type_name.is_empty() {
                c.type_name = to_pascal_case(&c.config_name);
            }
            for a in &mut c.attributes {
                if a.field_name.is_empty() {
                    a.field_name = to_camel_case(&a.name);
                }
            }
        }
        for e in self
            .enums
            .iter_mut()
            .filter(|e| !e.key.starts_with("check:"))
        {
            if e.type_name.is_empty() {
                e.type_name = to_pascal_case(&e.config_name);
            }
        }
        for d in &mut self.domains {
            if d.type_name.is_empty() {
                d.type_name = to_pascal_case(&d.config_name);
            }
        }
        let mut func_name_counts: HashMap<String, usize> = HashMap::new();
        for f in &self.functions {
            *func_name_counts.entry(f.name.clone()).or_default() += 1;
        }
        for f in &mut self.functions {
            let conflict = func_name_counts.get(&f.name).copied().unwrap_or(0) > 1;
            if f.field_name.is_empty() {
                let base = tvf_root_query_name(&f.name);
                f.field_name = if conflict {
                    format!("{}{}", to_camel_case(&f.schema), to_pascal_case(&base))
                } else {
                    base
                };
            }
            if let Some(parent) = &f.parent_table {
                if f.mounted_field_name.is_empty() {
                    f.mounted_field_name = tvf_mounted_field_name(&f.name, parent);
                }
            }
        }
    }

    /// Removes a table and everything that only makes sense with it: foreign keys pointing at
    /// it, functions mounted on it, computed columns, many-to-many relations and exclusive
    /// arcs through it, and its polymorphic hierarchies.
    pub fn remove_table(&mut self, schema: &str, table: &str) {
        let is = |s: &str, t: &str| s == schema && t == table;
        self.tables.retain(|t| !is(&t.schema, &t.name));
        for t in &mut self.tables {
            t.foreign_keys
                .retain(|fk| !is(&fk.foreign_schema, &fk.foreign_table));
        }
        self.functions.retain(|f| {
            f.parent_schema.as_deref().unwrap_or(&f.schema) != schema
                || f.parent_table.as_deref() != Some(table)
        });
        self.many_to_many
            .retain(|m| m.source_table != table && m.target_table != table);
        self.exclusive_arcs
            .retain(|a| a.table_name != table && !a.target_tables.iter().any(|t| t == table));
        self.search_indexes
            .retain(|i| !is(&i.table_schema, &i.table_name));
        self.sti.retain(|s| !is(&s.schema, &s.table_name));
        self.cti.retain(|c| !is(&c.schema, &c.parent_table));
        for c in &mut self.cti {
            c.children.retain(|ch| !is(&ch.schema, &ch.table));
        }
        self.invalidate_index();
    }

    /// Final pass after inspection: normalises the detected hierarchies and assigns
    /// collision-free GraphQL names.
    ///
    /// - Drops hierarchies on unknown tables.
    /// - A table is either polymorphic (an interface) or a concrete type: a CTI parent that is
    ///   itself a CTI child keeps only its child role (INHERITS descendants are already listed
    ///   under the root), and STI is dropped on CTI parents and children.
    /// - STI variant names come from discriminator values (`CHECKLIST_ITEM` -> `ChecklistItem`).
    ///   If any variant of a hierarchy would clash with another type, every variant of that
    ///   hierarchy is prefixed with the base type (`SingleTableItemPost`).
    /// - Fallback concrete types are `Generic<Base>`.
    pub fn resolve_polymorphic_names(&mut self) {
        let tables = &self.tables;
        let known =
            |schema: &str, name: &str| tables.iter().any(|t| t.schema == schema && t.name == name);
        self.cti.retain(|c| known(&c.schema, &c.parent_table));
        for c in &mut self.cti {
            c.children.retain(|ch| known(&ch.schema, &ch.table));
        }
        let is_child = |cti: &[CtiMetadata], schema: &str, table: &str| {
            cti.iter().any(|c| {
                c.children
                    .iter()
                    .any(|ch| ch.schema == schema && ch.table == table)
            })
        };
        let snapshot = self.cti.clone();
        self.cti
            .retain(|c| !c.children.is_empty() && !is_child(&snapshot, &c.schema, &c.parent_table));
        let cti = &self.cti;
        self.sti.retain(|s| {
            !s.variants.is_empty()
                && known(&s.schema, &s.table_name)
                && !cti
                    .iter()
                    .any(|c| c.schema == s.schema && c.parent_table == s.table_name)
                && !is_child(cti, &s.schema, &s.table_name)
        });
        let mut seen = HashSet::new();
        self.sti
            .retain(|s| seen.insert((s.schema.clone(), s.table_name.clone())));

        let type_name = |schema: &str, table: &str| {
            tables
                .iter()
                .find(|t| t.schema == schema && t.name == table)
                .map(|t| t.type_name.clone())
                .unwrap_or_else(|| table_to_type_name(table))
        };
        let mut taken: HashSet<String> = self.reserved_type_names();

        for c in &mut self.cti {
            c.base_type_name = type_name(&c.schema, &c.parent_table);
            c.fallback_type_name = claim_name(&mut taken, &format!("Generic{}", c.base_type_name));
            for ch in &mut c.children {
                ch.type_name = type_name(&ch.schema, &ch.table);
            }
        }
        for s in &mut self.sti {
            let base = type_name(&s.schema, &s.table_name);
            let nullable = tables
                .iter()
                .find(|t| t.schema == s.schema && t.name == s.table_name)
                .and_then(|t| t.get_column(&s.discriminator_column))
                .is_none_or(|c| c.is_nullable);
            s.fallback_type_name =
                nullable.then(|| claim_name(&mut taken, &format!("Generic{}", base)));

            let mut bare: Vec<String> = s
                .variants
                .iter()
                .map(|v| graphql_type_name(&v.discriminator_value))
                .collect();
            let unique = bare.iter().collect::<HashSet<_>>().len() == bare.len();
            let prefix = !unique
                || bare.iter().any(|n| {
                    taken.contains(n) || !n.starts_with(|c: char| c.is_ascii_alphabetic())
                });
            if prefix {
                bare = bare
                    .iter()
                    .map(|n| format!("{}{}", base, n.trim_start_matches('_')))
                    .collect();
            }
            for (v, name) in s.variants.iter_mut().zip(bare) {
                v.type_name = claim_name(&mut taken, &name);
            }
            s.base_type_name = base;
        }
        for arc in &mut self.exclusive_arcs {
            arc.target_type_names = arc
                .target_tables
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    type_name(
                        arc.target_schemas.get(i).map(String::as_str).unwrap_or(""),
                        t,
                    )
                })
                .collect();
        }
        self.invalidate_index();
    }

    /// Names enum and composite types, collision-free: `CHECK` enums not named by the config
    /// become `<Type><Field>` of their column (`AnimalKind`); names clashing with other types
    /// get a numeric suffix.
    pub fn resolve_value_type_names(&mut self) {
        for i in 0..self.enums.len() {
            if !self.enums[i].type_name.is_empty() {
                continue;
            }
            let name = self.enums[i]
                .key
                .strip_prefix("check:")
                .and_then(|k| k.split_once('.'))
                .and_then(|(table, column)| {
                    let t = self.tables.iter().find(|t| t.name == table)?;
                    Some(format!(
                        "{}{}",
                        t.type_name,
                        to_pascal_case(&t.get_column(column)?.field_name)
                    ))
                })
                .unwrap_or_else(|| to_pascal_case(&self.enums[i].config_name));
            self.enums[i].type_name = name;
        }
        let mut taken = self.reserved_type_names_except_values();
        for c in &mut self.composites {
            let mut name = claim_name(&mut taken, &c.type_name);
            while taken.contains(&format!("{name}Input")) {
                name = claim_name(&mut taken, &c.type_name);
            }
            taken.insert(format!("{name}Input"));
            c.type_name = name;
        }
        for e in &mut self.enums {
            e.type_name = claim_name(&mut taken, &e.type_name);
            for suffix in ["Filter", "ListFilter"] {
                taken.insert(format!("{}{suffix}", e.type_name));
            }
        }
        for d in &mut self.domains {
            d.type_name = claim_name(&mut taken, &d.type_name);
        }
        self.invalidate_index();
    }

    /// Every GraphQL type name the schema builder emits apart from polymorphic ones.
    fn reserved_type_names(&self) -> HashSet<String> {
        let mut names = self.reserved_type_names_except_values();
        for c in &self.composites {
            names.insert(c.type_name.clone());
            names.insert(c.input_type_name());
        }
        for e in &self.enums {
            for suffix in ["", "Filter", "ListFilter"] {
                names.insert(format!("{}{suffix}", e.type_name));
            }
        }
        names.extend(self.domains.iter().map(|d| d.type_name.clone()));
        names
    }

    /// Reserved names of everything but enum and composite types.
    fn reserved_type_names_except_values(&self) -> HashSet<String> {
        const BUILTINS: &[&str] = &[
            "Query",
            "Mutation",
            "Subscription",
            "Node",
            "PageInfo",
            "JSON",
            "String",
            "Int",
            "Float",
            "Boolean",
            "ID",
            "ConflictAction",
            "LTreeFilter",
            "FullTextFilter",
            "GeoJSONFilter",
            "GeoJSONDistanceInput",
            "BoundingBoxInput",
            "GlobalSearchResults",
            "GlobalSearchHit",
            "GlobalSearchNode",
        ];
        const TABLE_SUFFIXES: &[&str] = &[
            "",
            "Condition",
            "Filter",
            "OrderBy",
            "Edge",
            "Connection",
            "Aggregates",
            "MinAggregate",
            "MaxAggregate",
            "SumAggregate",
            "AvgAggregate",
            "FuzzyField",
            "FuzzyInput",
            "VectorField",
            "SemanticInput",
            "Bm25Field",
            "FullTextField",
            "SpatialField",
            "Bm25Input",
            "HybridSearchInput",
            "OnConflict",
            "ConflictKey",
            "UpdateColumn",
            "KeyInput",
            "ToOneInput",
            "ToManyInput",
            "GroupBy",
            "AggregateGroup",
            "HavingInput",
            "HavingNumbersInput",
            "ToOneUpdateInput",
            "ToManyUpdateInput",
            "ErrorCode",
            "ToManyFilter",
            "DistinctCountAggregate",
            "StatisticAggregate",
            "ConnectOrCreateInput",
            "UpdateByKeyInput",
        ];
        let mut names: HashSet<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
        for scalar in ["String", "Int", "Float", "Boolean"]
            .iter()
            .chain(CUSTOM_SCALARS.iter().map(|(name, _)| name))
        {
            names.insert(scalar.to_string());
            for suffix in ["Filter", "ListFilter"] {
                names.insert(format!("{scalar}{suffix}"));
            }
        }
        for scalar in RANGE_SCALARS {
            for suffix in [
                "Range",
                "RangeInput",
                "RangeBound",
                "RangeBoundInput",
                "RangeFilter",
                "MultirangeFilter",
            ] {
                names.insert(format!("{scalar}{suffix}"));
            }
        }
        for t in &self.tables {
            let ty = &t.type_name;
            for suffix in TABLE_SUFFIXES {
                names.insert(format!("{ty}{suffix}"));
            }
            for action in MutationAction::ALL {
                names.insert(format!("{}{ty}Input", action.type_prefix()));
                names.insert(action.payload_type_name(t));
            }
        }
        for f in self.functions.iter().filter(|f| f.returns_record()) {
            names.insert(f.row_type_name());
        }
        for a in &self.exclusive_arcs {
            names.insert(a.union_name.clone());
        }
        names
    }

    /// Column a search index searches, if it is exposed (hidden columns are not searchable).
    pub fn search_column(&self, idx: &SearchIndexMetadata) -> Option<&ColumnMetadata> {
        self.find_table(&idx.table_schema, &idx.table_name)?
            .get_column(&idx.column_name)
            .filter(|c| !c.omit)
    }

    /// Value naming a search index in `<Type>FuzzyField`-style enums: its column's `orderBy`
    /// name, or `ID` for an index without a plain column.
    pub fn search_field_value(&self, idx: &SearchIndexMetadata) -> String {
        match self.search_column(idx) {
            Some(col) => col.order_by_name(),
            // An expression index, by the columns it reads
            None if !idx.columns.is_empty() => {
                let table = self.find_table(&idx.table_schema, &idx.table_name);
                let names: Vec<String> = idx
                    .columns
                    .iter()
                    .map(|c| {
                        table
                            .and_then(|t| t.get_column(c))
                            .map(|c| c.order_by_name())
                            .unwrap_or_else(|| enum_value_name(c))
                    })
                    .collect();
                names.join("_AND_")
            }
            None => "ID".to_string(),
        }
    }

    /// Drops search indexes on hidden columns (indexes without a plain column, such as BM25
    /// indexes over several fields, stay).
    pub fn prune_hidden_search_indexes(&mut self) {
        let hidden: Vec<bool> = self
            .search_indexes
            .iter()
            .map(|i| !i.column_name.is_empty() && self.search_column(i).is_none())
            .collect();
        let mut hidden = hidden.into_iter();
        self.search_indexes
            .retain(|_| !hidden.next().unwrap_or(false));
    }

    pub fn search_indexes_for_table(&self, schema: &str, table: &str) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| idx.table_schema == schema && idx.table_name == table)
            .collect()
    }

    pub fn fuzzy_indexes_for_table(&self, schema: &str, table: &str) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| {
                idx.table_schema == schema
                    && idx.table_name == table
                    && matches!(idx.kind, SearchIndexKind::FuzzyTrgm)
            })
            .collect()
    }

    pub fn vector_indexes_for_table(&self, schema: &str, table: &str) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| {
                idx.table_schema == schema
                    && idx.table_name == table
                    && matches!(idx.kind, SearchIndexKind::Vector { .. })
            })
            .collect()
    }

    pub fn bm25_indexes_for_table(&self, schema: &str, table: &str) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| {
                idx.table_schema == schema
                    && idx.table_name == table
                    && matches!(idx.kind, SearchIndexKind::Bm25)
            })
            .collect()
    }

    pub fn full_text_indexes_for_table(
        &self,
        schema: &str,
        table: &str,
    ) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| {
                idx.table_schema == schema
                    && idx.table_name == table
                    && matches!(idx.kind, SearchIndexKind::FullText { .. })
            })
            .collect()
    }

    pub fn spatial_indexes_for_table(
        &self,
        schema: &str,
        table: &str,
    ) -> Vec<&SearchIndexMetadata> {
        self.search_indexes
            .iter()
            .filter(|idx| {
                idx.table_schema == schema
                    && idx.table_name == table
                    && idx.kind == SearchIndexKind::Spatial
            })
            .collect()
    }

    /// Tables `globalSearch` searches, with the index each is searched by: a full-text index,
    /// else a trigram index. Searching reads rows, so they need the list or connection
    /// operation.
    pub fn global_search_tables(&self) -> Vec<(&TableMetadata, &SearchIndexMetadata)> {
        self.tables
            .iter()
            .filter(|t| t.operations.list || t.operations.connection)
            .filter_map(|t| {
                let full_text = self
                    .full_text_indexes_for_table(&t.schema, &t.name)
                    .into_iter()
                    .next();
                let fuzzy = || {
                    self.fuzzy_indexes_for_table(&t.schema, &t.name)
                        .into_iter()
                        .find(|i| self.search_column(i).is_some())
                };
                Some((t, full_text.or_else(fuzzy)?))
            })
            .collect()
    }

    /// Name of the global search root field of `schema`, if any table is searchable and no
    /// table's root field takes the name.
    pub fn global_search_field(&self, schema: &str) -> Option<&'static str> {
        self.index()
            .global_search_schemas
            .contains(schema)
            .then_some(GLOBAL_SEARCH_FIELD)
    }

    /// Name of the global search root field, if any inspected schema has one (the search
    /// itself spans every schema's searchable tables).
    pub fn global_search_field_any(&self) -> Option<&'static str> {
        (!self.index().global_search_schemas.is_empty()).then_some(GLOBAL_SEARCH_FIELD)
    }

    /// Whether `type_name` is a member of the `GlobalSearchNode` union.
    pub fn is_global_search_type(&self, type_name: &str) -> bool {
        self.index().global_search_types.contains(type_name)
    }

    /// Concrete types of the `GlobalSearchNode` union (polymorphic tables contribute theirs).
    pub fn global_search_types(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (t, _) in self.global_search_tables() {
            let names = match self.find_polymorphism(&t.schema, &t.name) {
                Some(poly) => poly.member_type_names(),
                None => vec![t.type_name.clone()],
            };
            for name in names {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Hybrid search ranks rows by their single-column key.
    pub fn has_hybrid_search(&self, schema: &str, table: &str) -> bool {
        if !self
            .find_table(schema, table)
            .is_some_and(|t| t.implements_node())
        {
            return false;
        }
        let has_vec = !self.vector_indexes_for_table(schema, table).is_empty();
        let has_lex = !self.fuzzy_indexes_for_table(schema, table).is_empty()
            || !self.bm25_indexes_for_table(schema, table).is_empty();
        has_vec && has_lex
    }

    /// Values of `table`'s `<Type>OrderBy` enum, without their `_ASC` / `_DESC` suffix, and what
    /// each sorts by: its orderable columns, the orderable columns of the rows its to-one
    /// relations lead to (`AUTHOR_NAME`), the number of rows of its to-many relations
    /// (`MESSAGES_COUNT`), and its computed columns returning an orderable value that need
    /// no other argument. The first value claiming a name wins.
    pub fn order_options(&self, table: &TableMetadata) -> Vec<(String, OrderTarget)> {
        let mut out: Vec<(String, OrderTarget)> = Vec::new();
        let mut push = |name: String, target: OrderTarget| {
            if !matches!(name.as_str(), "NATURAL" | "PRIMARY_KEY")
                && !out.iter().any(|(n, _)| *n == name)
            {
                out.push((name, target));
            }
        };
        for col in table.visible_columns().filter(|c| c.pg_type.is_orderable()) {
            push(col.order_by_name(), OrderTarget::Column(col.name.clone()));
        }
        let mut seen: Vec<&RelationInfo> = Vec::new();
        let fields = self.object_fields(&table.schema, &table.name);
        for field in fields {
            match &field.kind {
                FieldKind::Relation(rel) if !seen.contains(&rel) => {
                    seen.push(rel);
                    let prefix = enum_value_name(&field.name);
                    if rel.is_many {
                        if !rel.history {
                            push(format!("{prefix}_COUNT"), OrderTarget::Count(rel.clone()));
                        }
                    } else if rel.junction.is_none() {
                        let Some(target) = self.find_table(&rel.target_schema, &rel.target_table)
                        else {
                            continue;
                        };
                        for col in target
                            .visible_columns()
                            .filter(|c| c.pg_type.is_orderable())
                        {
                            let target_ = OrderTarget::Related {
                                rel: rel.clone(),
                                column: col.name.clone(),
                            };
                            push(format!("{prefix}_{}", col.order_by_name()), target_);
                        }
                    }
                }
                FieldKind::Function(i) => {
                    let f = &self.functions[*i];
                    let orderable = match &f.returns {
                        FunctionReturn::Value(t) => t.is_orderable() && !t.is_array(),
                        _ => false,
                    };
                    let no_other_args = f.arguments.iter().skip(1).all(|a| a.has_default);
                    if f.row_argument && orderable && !f.returns_set && no_other_args {
                        push(enum_value_name(&field.name), OrderTarget::Function(*i));
                    }
                }
                _ => {}
            }
        }
        // The columns of the subtypes of a table-per-type hierarchy (`SEATS`): the subtype's
        // row is the related row, and a parent without one sorts last
        for (_, members) in self.cti_columns(table) {
            let (child, col) = members[0];
            if col.pg_type.is_orderable() {
                let rel = RelationInfo {
                    is_many: false,
                    target_schema: child.schema.clone(),
                    target_table: child.table.clone(),
                    columns: child.join_columns.clone(),
                    junction: None,
                    period: None,
                    history: false,
                };
                push(
                    col.order_by_name(),
                    OrderTarget::Related {
                        rel,
                        column: col.name.clone(),
                    },
                );
            }
        }
        out
    }

    /// Relation field `field_name` of `(schema, table_name)`: a to-one or to-many relation
    /// (including many-to-many), or a connection over one.
    pub fn find_relation(
        &self,
        schema: &str,
        table_name: &str,
        field_name: &str,
    ) -> Option<RelationInfo> {
        match &self.object_field(schema, table_name, field_name)?.kind {
            FieldKind::Relation(rel) | FieldKind::Connection(rel) => Some(rel.clone()),
            _ => None,
        }
    }

    /// Fields of `table`'s object type, in order: Relay `id` and `databaseId` (single-column
    /// primary key), columns, to-one relations (per foreign key: `<column without _id>` and
    /// `<targetType>`, or the configured name), to-many relations and their connections,
    /// many-to-many relations, the exclusive-arc `target`, mounted functions and computed
    /// columns. The first field claiming a name wins.
    fn build_object_fields(&self, table: &TableMetadata) -> Vec<ObjectField> {
        let mut fields: Vec<ObjectField> = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        let mut push = |name: String, kind: FieldKind| {
            if taken.insert(name.clone()) {
                fields.push(ObjectField { name, kind });
            }
        };
        let known = |schema: &str, name: &str| self.find_table(schema, name).is_some();

        let node = table.implements_node();
        if node {
            push("id".to_string(), FieldKind::GlobalId);
            if table
                .get_column(&table.primary_key[0])
                .is_some_and(|c| !c.omit)
            {
                push("databaseId".to_string(), FieldKind::DatabaseId);
            }
        }
        for col in table.visible_columns() {
            if node && col.is_primary_key {
                continue;
            }
            push(col.field_name.clone(), FieldKind::Column(col.name.clone()));
        }
        if table.has_row_version() {
            push("rowVersion".to_string(), FieldKind::RowVersion);
        }

        // Outgoing foreign keys (N:1)
        for fk in &table.foreign_keys {
            if fk.omit || !known(&fk.foreign_schema, &fk.foreign_table) {
                continue;
            }
            let (pairs, period) = fk.split_period();
            let rel = RelationInfo {
                is_many: false,
                target_schema: fk.foreign_schema.clone(),
                target_table: fk.foreign_table.clone(),
                columns: pairs.into_iter().map(|(c, f)| (f, c)).collect(),
                junction: None,
                period: period.map(|(c, f)| (f, c)),
                history: false,
            };
            let names = match &fk.field_name {
                Some(name) => vec![name.clone()],
                None => {
                    let mut names = Vec::new();
                    if let Some((col, _)) = fk.single() {
                        if let Some(base) = col.strip_suffix("_id") {
                            names.push(to_camel_case(base));
                        }
                    }
                    names.push(to_camel_case(
                        &self.type_name_of(&fk.foreign_schema, &fk.foreign_table),
                    ));
                    names
                }
            };
            for name in names {
                push(name, FieldKind::Relation(rel.clone()));
            }
        }

        // Incoming foreign keys (1:N)
        for other in &self.tables {
            for fk in &other.foreign_keys {
                if fk.foreign_schema != table.schema
                    || fk.foreign_table != table.name
                    || fk.omit
                    || fk.omit_reverse
                {
                    continue;
                }
                let (columns, period) = fk.split_period();
                let rel = RelationInfo {
                    is_many: true,
                    target_schema: other.schema.clone(),
                    target_table: other.name.clone(),
                    columns,
                    junction: None,
                    period,
                    history: false,
                };
                let other_conflict =
                    self.tables.iter().filter(|t| t.name == other.name).count() > 1;
                let name = fk.reverse_field_name.clone().unwrap_or_else(|| {
                    if other_conflict {
                        other.list_field.clone()
                    } else {
                        to_camel_case(&other.name)
                    }
                });
                push(name.clone(), FieldKind::Relation(rel.clone()));
                push(format!("{name}Connection"), FieldKind::Connection(rel));
            }
        }

        // Many-to-many relations through junction tables
        for m2m in self.many_to_many.iter().filter(|m| {
            (m.source_schema.is_empty() || m.source_schema == table.schema)
                && m.source_table == table.name
        }) {
            let target_schema = if m2m.target_schema.is_empty() {
                &table.schema
            } else {
                &m2m.target_schema
            };
            let Some(target) = self.find_table(target_schema, &m2m.target_table) else {
                continue;
            };
            let rel = RelationInfo {
                is_many: true,
                target_schema: target.schema.clone(),
                target_table: target.name.clone(),
                columns: Vec::new(),
                junction: Some(JunctionRelationInfo {
                    junction_schema: m2m.junction_schema.clone(),
                    junction_table: m2m.junction_table.clone(),
                    source_columns: m2m.junction_source.clone(),
                    target_columns: m2m.junction_target.clone(),
                }),
                period: None,
                history: false,
            };
            for name in [&m2m.field_name, &m2m.via_field_name] {
                push(name.clone(), FieldKind::Relation(rel.clone()));
                push(
                    format!("{name}Connection"),
                    FieldKind::Connection(rel.clone()),
                );
            }
        }

        // Hierarchies: foreign keys referencing the table itself (the first gets plain
        // `ancestors` / `descendants`, others their to-one field's name as prefix), and
        // `ltree` path columns
        let mut self_keys = 0;
        for fk in &table.foreign_keys {
            if fk.omit
                || fk.period
                || fk.foreign_schema != table.schema
                || fk.foreign_table != table.name
            {
                continue;
            }
            let pairs: Vec<(String, String)> = fk
                .columns
                .iter()
                .cloned()
                .zip(fk.foreign_columns.iter().cloned())
                .collect();
            let prefix = match (self_keys, &fk.field_name, fk.single()) {
                (0, _, _) => String::new(),
                (_, Some(name), _) => name.clone(),
                (_, None, Some((col, _))) => to_camel_case(col.strip_suffix("_id").unwrap_or(col)),
                (_, None, None) => to_camel_case(&fk.constraint_name),
            };
            self_keys += 1;
            for (ancestors, suffix) in [(true, "ancestors"), (false, "descendants")] {
                let name = if prefix.is_empty() {
                    suffix.to_string()
                } else {
                    format!("{prefix}{}", to_pascal_case(suffix))
                };
                push(
                    name,
                    FieldKind::Tree(TreeInfo {
                        ancestors,
                        link: TreeLink::ForeignKey(pairs.clone()),
                    }),
                );
            }
        }
        for col in table
            .visible_columns()
            .filter(|c| c.pg_type == PgType::Ltree)
        {
            for (ancestors, suffix) in [(true, "Ancestors"), (false, "Descendants")] {
                push(
                    format!("{}{suffix}", col.field_name),
                    FieldKind::Tree(TreeInfo {
                        ancestors,
                        link: TreeLink::Path(col.name.clone()),
                    }),
                );
            }
        }

        // Versions of the same entity, for tables with a temporal key
        if let Some(key) = table.period_key() {
            let rel = RelationInfo {
                is_many: true,
                target_schema: table.schema.clone(),
                target_table: table.name.clone(),
                columns: key.entity.iter().map(|c| (c.clone(), c.clone())).collect(),
                junction: None,
                period: None,
                history: true,
            };
            push("history".to_string(), FieldKind::Relation(rel.clone()));
            push("historyConnection".to_string(), FieldKind::Connection(rel));
        }

        if self
            .exclusive_arcs
            .iter()
            .any(|a| a.table_name == table.name)
        {
            push("target".to_string(), FieldKind::ArcTarget);
        }
        for (i, func) in self.functions.iter().enumerate() {
            if func.is_mounted_on(table) && self.function_is_exposed(func) {
                push(func.mounted_field_name.clone(), FieldKind::Function(i));
            }
        }
        fields
    }
}

/// camelCase of an SQL name. Characters that cannot appear in a GraphQL name (`_`, `-`,
/// spaces, quotes, ...) separate words; a leading digit gets a `_` prefix.
pub fn to_camel_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize = false;
    for (i, c) in s.chars().enumerate() {
        if !c.is_ascii_alphanumeric() {
            capitalize = !result.is_empty();
        } else if capitalize {
            result.push(c.to_ascii_uppercase());
            capitalize = false;
        } else if i == 0 {
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    prefix_digit(result)
}

fn prefix_digit(name: String) -> String {
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{name}")
    } else {
        name
    }
}

/// GraphQL type name for a discriminator value: `dog` -> `Dog`, `CHECKLIST_ITEM` -> `ChecklistItem`,
/// `lab-result` -> `LabResult`. Characters invalid in GraphQL names act as word separators.
/// The result may start with a digit; callers prefix it in that case.
pub fn graphql_type_name(value: &str) -> String {
    let has_lower = value.chars().any(|c| c.is_lowercase());
    let normalized: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                if has_lower {
                    c
                } else {
                    c.to_ascii_lowercase()
                }
            } else {
                '_'
            }
        })
        .collect();
    let name = to_pascal_case(&normalized);
    if name.is_empty() {
        "_".to_string()
    } else {
        name
    }
}

/// Hints in an SQL comment, each on a line of its own: `@name <GraphQL name>` names the
/// object in the schema, `@deprecated [reason]` deprecates its fields. Optional, and an
/// alternative to the config (which wins over them): versioned with the migrations that
/// create the objects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentHints {
    /// The comment without its hint lines (`None` if nothing is left).
    pub description: Option<String>,
    pub name: Option<String>,
    pub deprecated: Option<String>,
}

impl CommentHints {
    pub fn parse(comment: Option<&str>) -> Self {
        let Some(comment) = comment else {
            return Self::default();
        };
        let mut hints = Self::default();
        let mut kept: Vec<&str> = Vec::new();
        for line in comment.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed
                .strip_prefix("@deprecated")
                .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
            {
                let reason = rest.trim();
                hints.deprecated = Some(if reason.is_empty() {
                    "No longer supported.".to_string()
                } else {
                    reason.to_string()
                });
            } else if let Some(name) = trimmed
                .strip_prefix("@name ")
                .map(str::trim)
                .filter(|n| is_graphql_name(n))
            {
                hints.name = Some(name.to_string());
            } else {
                kept.push(line);
            }
        }
        let description = kept.join("\n").trim().to_string();
        hints.description = (!description.is_empty()).then_some(description);
        hints
    }
}

impl DatabaseCatalog {
    /// Applies the `@name` / `@deprecated` hints of tables', columns', functions' and types'
    /// comments (see [`CommentHints`]), before default names are assigned.
    pub fn apply_comment_hints(&mut self) {
        for t in &mut self.tables {
            let hints = CommentHints::parse(t.description.as_deref());
            t.description = hints.description;
            if let Some(name) = hints.name {
                t.type_name = name;
            }
            t.deprecation = hints.deprecated;
            for c in &mut t.columns {
                let hints = CommentHints::parse(c.description.as_deref());
                c.description = hints.description;
                if let Some(name) = hints.name {
                    c.field_name = name;
                }
                c.deprecation = hints.deprecated;
            }
        }
        for f in &mut self.functions {
            let hints = CommentHints::parse(f.description.as_deref());
            f.description = hints.description;
            if let Some(name) = hints.name {
                if f.row_argument {
                    f.mounted_field_name = name;
                } else {
                    f.field_name = name;
                }
            }
            f.deprecation = hints.deprecated;
        }
        for c in &mut self.composites {
            let hints = CommentHints::parse(c.description.as_deref());
            c.description = hints.description;
            if let Some(name) = hints.name {
                c.type_name = name;
            }
        }
        for e in &mut self.enums {
            let hints = CommentHints::parse(e.description.as_deref());
            e.description = hints.description;
            if let Some(name) = hints.name {
                e.type_name = name;
            }
        }
        for d in &mut self.domains {
            let hints = CommentHints::parse(d.description.as_deref());
            d.description = hints.description;
            if let Some(name) = hints.name {
                d.type_name = name;
            }
        }
        self.invalidate_index();
    }
}

/// `@deprecated(reason: "...")` for a deprecation reason, or nothing.
pub fn deprecated_directive(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => {
            let escaped = reason
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            format!(" @deprecated(reason: \"{escaped}\")")
        }
        None => String::new(),
    }
}

/// Whether `name` is a valid GraphQL name (`/[_A-Za-z][_0-9A-Za-z]*/`).
pub fn is_graphql_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Claims `wanted` in `taken`, appending a numeric suffix until the name is free.
fn claim_name(taken: &mut HashSet<String>, wanted: &str) -> String {
    claim_name_with(taken, wanted, "")
}

/// Like [`claim_name`], with `separator` before the suffix (`EMAIL_TAKEN_2`).
fn claim_name_with(taken: &mut HashSet<String>, wanted: &str, separator: &str) -> String {
    let mut name = wanted.to_string();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{wanted}{separator}{n}");
        n += 1;
    }
    taken.insert(name.clone());
    name
}

/// The part of a constraint's name its author chose, or `None` for a name PostgreSQL
/// generated (`<table>_<columns>_<label>`, possibly with a number appended): then the code
/// comes from the columns. The table-name prefix of a chosen name is left out.
fn custom_constraint_name<'a>(name: &'a str, table: &str, label: &str) -> Option<&'a str> {
    let rest = name.strip_prefix(table).and_then(|r| r.strip_prefix('_'));
    if let Some(rest) = rest {
        let generated = rest
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .strip_suffix(label)
            .is_some_and(|r| r.ends_with('_') || r.is_empty());
        if generated {
            return None;
        }
        return Some(rest);
    }
    Some(name)
}

/// PascalCase of an SQL name; word separators as in [`to_camel_case`].
pub fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize = true;
    for c in s.chars() {
        if !c.is_ascii_alphanumeric() {
            capitalize = true;
        } else if capitalize {
            result.push(c.to_ascii_uppercase());
            capitalize = false;
        } else {
            result.push(c);
        }
    }
    prefix_digit(result)
}

pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

// ---- Default naming ---------------------------------------------------------------------
// Applied once by `DatabaseCatalog::assign_default_names`; everything else reads the names
// stored in the catalog, so configured names apply everywhere.

/// List field, e.g. `order_items` -> `orderItems`.
pub fn list_field_name(table_name: &str) -> String {
    to_camel_case(table_name)
}

/// Single-record-by-PK field, e.g. `order_items` -> `orderItem`. Tables whose singular equals
/// the list name (e.g. `series_of_data`) get a `ById` suffix to avoid a collision.
pub fn single_field_name(table_name: &str) -> String {
    let single = to_camel_case(&table_to_type_name(table_name));
    let list = list_field_name(table_name);
    if single == list {
        format!("{}ById", list)
    } else {
        single
    }
}

/// Root connection fields: `orderItemsConnection` and `allOrderItemsConnection`.
pub fn connection_field_names(table_name: &str) -> (String, String) {
    (
        format!("{}Connection", list_field_name(table_name)),
        format!("all{}Connection", to_pascal_case(table_name)),
    )
}

pub fn table_to_type_name(table_name: &str) -> String {
    let singular = if let Some(stem) = table_name.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = table_name
        .strip_suffix('s')
        .filter(|_| !table_name.ends_with("ss"))
    {
        stem.to_string()
    } else {
        table_name.to_string()
    };

    to_pascal_case(&singular)
}

/// Singular snake_case name of a table: `care_plans` -> `care_plan`, `users` -> `user`.
pub fn table_singular_snake(table_name: &str) -> String {
    to_snake_case(&table_to_type_name(table_name))
}

/// Mounted field name on a parent table object type.
///
/// Generic rule: drop the function name up to and including the parent's singular name
/// (the parent is implied by where the field is mounted), then camelCase the remainder.
/// Examples:
/// - `user_summary` on `users`             → `summary`
/// - `export_order_totals` on `orders`     → `totals`
/// - `list_care_plan_goals` on `care_plans` → `goals`
/// - `generate_report` on `users`          → `generateReport`  (no match → full camelCase)
pub fn tvf_mounted_field_name(func_name: &str, parent_table: &str) -> String {
    let words: Vec<&str> = func_name.split('_').collect();
    let parent: Vec<String> = table_singular_snake(parent_table)
        .split('_')
        .map(str::to_lowercase)
        .collect();
    let rest = (0..words.len().saturating_sub(parent.len()))
        .find(|&i| {
            words[i..i + parent.len()]
                .iter()
                .zip(&parent)
                .all(|(w, p)| w.eq_ignore_ascii_case(p))
        })
        .map(|i| words[i + parent.len()..].join("_"));
    match rest {
        Some(rest) if !rest.is_empty() => to_camel_case(&rest),
        _ => to_camel_case(func_name),
    }
}

/// Root Query field name for a table-valued function.
///
/// Generic rule: camelCase the whole function name as-is. Examples:
/// - `user_match`      → `userMatch`
/// - `generate_report` → `generateReport`
pub fn tvf_root_query_name(func_name: &str) -> String {
    to_camel_case(func_name)
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    #[test]
    fn mounted_field_name_drops_parent_prefix() {
        assert_eq!(tvf_mounted_field_name("user_summary", "users"), "summary");
        assert_eq!(
            tvf_mounted_field_name("export_order_totals", "orders"),
            "totals"
        );
        assert_eq!(
            tvf_mounted_field_name("list_care_plan_goals", "care_plans"),
            "goals"
        );
        assert_eq!(
            tvf_mounted_field_name("generate_report", "users"),
            "generateReport"
        );
        assert_eq!(tvf_mounted_field_name("user", "users"), "user");
    }

    #[test]
    fn discriminator_values_become_type_names() {
        assert_eq!(graphql_type_name("dog"), "Dog");
        assert_eq!(graphql_type_name("CHECKLIST_ITEM"), "ChecklistItem");
        assert_eq!(graphql_type_name("lab-result"), "LabResult");
        assert_eq!(graphql_type_name("camelCase"), "CamelCase");
    }

    #[test]
    fn array_types_and_literals() {
        assert_eq!(
            PgType::from_sql_type("_text"),
            PgType::Array(Box::new(PgType::Text))
        );
        assert_eq!(
            PgType::from_sql_type("_int8").sql_cast().as_deref(),
            Some("int8[]")
        );
        assert_eq!(PgType::from_sql_type("_int8").graphql_type(), "[BigInt]");
        assert_eq!(
            PgType::from_sql_type("_int8").output_cast().as_deref(),
            Some("text[]")
        );
        let lit = pg_array_literal(&serde_json::json!(["a", "b\"c", null, 3]));
        assert_eq!(lit, serde_json::json!("{\"a\",\"b\\\"c\",NULL,\"3\"}"));
    }

    #[test]
    fn enum_values_from_labels() {
        assert_eq!(enum_value_name("in-progress"), "IN_PROGRESS");
        assert_eq!(enum_value_name("entered-in-error"), "ENTERED_IN_ERROR");
        assert_eq!(enum_value_name("camelCase"), "CAMEL_CASE");
        assert_eq!(enum_value_name("TOPIC"), "TOPIC");
        assert_eq!(enum_value_name("1st"), "_1ST");
        assert_eq!(enum_value_name("a b--c"), "A_B_C");
        assert_eq!(enum_value_name(""), "_EMPTY");
        assert_eq!(enum_value_name("*"), "_EMPTY");
    }

    #[test]
    fn names_from_unusual_identifiers() {
        assert_eq!(to_camel_case("Mixed Case"), "mixedCase");
        assert_eq!(to_camel_case("quote\"col"), "quoteCol");
        assert_eq!(to_pascal_case("Odd \"Names\""), "OddNames");
        assert_eq!(to_camel_case("2fa_codes"), "_2faCodes");
        assert_eq!(to_camel_case("created_at"), "createdAt");
        assert_eq!(to_pascal_case("care_plans"), "CarePlans");
    }

    #[test]
    fn plural_type_names() {
        assert_eq!(pluralize("Forum"), "Forums");
        assert_eq!(pluralize("Category"), "Categories");
        assert_eq!(pluralize("Survey"), "Surveys");
        assert_eq!(pluralize("Address"), "Addresses");
        assert_eq!(pluralize("Box"), "Boxes");
        assert_eq!(pluralize("CarePlanActivity"), "CarePlanActivities");
    }

    #[test]
    fn comment_hints() {
        let hints = CommentHints::parse(Some(
            "Who wrote it.\n@name Writer\n@deprecated Use `authors` instead.",
        ));
        assert_eq!(hints.description.as_deref(), Some("Who wrote it."));
        assert_eq!(hints.name.as_deref(), Some("Writer"));
        assert_eq!(hints.deprecated.as_deref(), Some("Use `authors` instead."));
        let bare = CommentHints::parse(Some("@deprecated"));
        assert_eq!(
            (bare.description, bare.deprecated.as_deref()),
            (None, Some("No longer supported."))
        );
        // Not hints: an invalid name, a word merely starting with the tag
        let other = CommentHints::parse(Some("@name not valid\n@deprecatedness"));
        assert_eq!((other.name, other.deprecated), (None, None));
        assert_eq!(
            deprecated_directive(Some("say \"no\"")),
            " @deprecated(reason: \"say \\\"no\\\"\")"
        );
    }

    #[test]
    fn graphql_names() {
        assert!(is_graphql_name("dateOfBirth"));
        assert!(is_graphql_name("_x1"));
        assert!(!is_graphql_name("1x"));
        assert!(!is_graphql_name("a-b"));
        assert!(!is_graphql_name(""));
    }
}
