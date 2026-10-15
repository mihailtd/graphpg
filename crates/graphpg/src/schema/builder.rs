use crate::catalog::{
    conflict_key_value, deprecated_directive, pg_type_enum_key, to_camel_case, ColumnMetadata,
    DatabaseCatalog, FieldKind, FunctionArgument, FunctionMetadata, FunctionReturn, MutationAction,
    MutationField, OrderTarget, PgType, Polymorphism, SearchIndexKind, SearchIndexMetadata,
    SearchMethod, TableMetadata, TreeLink, CUSTOM_SCALARS, RANGE_SCALARS,
};
use apollo_compiler::validation::Valid;
use apollo_compiler::Schema;
use std::collections::{BTreeSet, HashSet};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SchemaBuildError {
    #[error("GraphQL schema validation error: {0}")]
    Validation(String),
}

pub struct SchemaBuilder;

/// One field of a generated object/interface type.
#[derive(Debug, Clone)]
pub struct SdlField {
    pub name: String,
    /// Rendered docstring (indented), possibly empty.
    pub doc: String,
    /// Everything after the name: optional arguments and the type, e.g. `(first: Int): XConnection!`.
    pub decl: String,
    /// Directives after the type (` @deprecated(...)`), possibly empty.
    pub directives: String,
}

impl SdlField {
    fn new(name: impl Into<String>, decl: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            doc: String::new(),
            decl: decl.into(),
            directives: String::new(),
        }
    }

    fn with_doc(mut self, doc: String) -> Self {
        self.doc = doc;
        self
    }

    fn deprecated(mut self, reason: Option<&str>) -> Self {
        self.directives = deprecated_directive(reason);
        self
    }

    fn render(fields: &[SdlField]) -> String {
        fields
            .iter()
            .map(|f| format!("{}  {}{}{}\n", f.doc, f.name, f.decl, f.directives))
            .collect()
    }

    /// Whether this field can implement `iface` (GraphQL IsValidImplementation): identical
    /// arguments and the same type, optionally made non-null.
    fn implements(&self, iface: &SdlField) -> bool {
        let split = |d: &str| {
            d.rsplit_once(": ")
                .map(|(a, t)| (a.to_string(), t.to_string()))
                .unwrap_or_default()
        };
        let (args, ty) = split(&self.decl);
        let (iface_args, iface_ty) = split(&iface.decl);
        args == iface_args && (ty == iface_ty || ty == format!("{iface_ty}!"))
    }
}

/// Comparison filter inputs: `(input name, value type, ordered)`. Ordered types also get
/// `lessThan`-style operators.
const COMPARISON_FILTERS: &[(&str, &str, bool)] = &[
    ("UUIDFilter", "UUID", false),
    ("IntFilter", "Int", true),
    ("BigIntFilter", "BigInt", true),
    ("FloatFilter", "Float", true),
    ("DecimalFilter", "Decimal", true),
    ("BooleanFilter", "Boolean", false),
    ("DatetimeFilter", "Datetime", true),
    ("DateFilter", "Date", true),
    ("TimeFilter", "Time", true),
    ("LocalDatetimeFilter", "LocalDatetime", true),
    ("TimeTzFilter", "TimeTz", true),
    ("IntervalFilter", "Interval", true),
    ("InternetAddressFilter", "InternetAddress", true),
    ("CidrAddressFilter", "CidrAddress", true),
    ("MacAddressFilter", "MacAddress", true),
];

impl SchemaBuilder {
    /// Builds a GraphQL Schema SDL string from an inspected DatabaseCatalog,
    /// then parses and validates it with apollo-compiler.
    pub fn build(catalog: &DatabaseCatalog) -> Result<Valid<Schema>, SchemaBuildError> {
        let sdl = Self::generate_sdl(catalog);
        let schema = Schema::parse_and_validate(&sdl, "schema.graphql")
            .map_err(|e| SchemaBuildError::Validation(e.to_string()))?;
        Ok(schema)
    }

    /// Fields of the GraphQL type generated for `table` from its own columns and relations
    /// (see `DatabaseCatalog::object_fields`).
    pub fn table_object_fields(catalog: &DatabaseCatalog, table: &TableMetadata) -> Vec<SdlField> {
        let type_of = |schema: &str, name: &str| catalog.type_name_of(schema, name);
        catalog
            .object_fields(&table.schema, &table.name)
            .iter()
            .filter_map(|field| {
                let name = field.name.clone();
                Some(match &field.kind {
                    FieldKind::GlobalId => SdlField::new(name, ": ID!").with_doc(
                        "  \"\"\"The globally unique identifier across all types for Relay compatibility.\"\"\"\n".to_string(),
                    ),
                    FieldKind::DatabaseId => {
                        let col = table.get_column(&table.primary_key[0])?;
                        SdlField::new(name, format!(": {}!", catalog.column_type(col, false)))
                            .with_doc("  \"\"\"The raw underlying database primary key value.\"\"\"\n".to_string())
                    }
                    FieldKind::Column(column) => {
                        let col = table.get_column(column)?;
                        let non_null = if col.is_nullable { "" } else { "!" };
                        // A JSON value, or the first item a JSON path selects from it
                        let args = if matches!(col.pg_type, PgType::Json | PgType::Jsonb) {
                            "(\"\"\"A JSON path (`$.address.city`): the first item it selects (null if none).\"\"\" path: String)"
                        } else {
                            ""
                        };
                        let non_null = if args.is_empty() { non_null } else { "" };
                        let mut field = SdlField::new(name, format!("{args}: {}{}", catalog.column_type(col, false), non_null))
                            .with_doc(Self::format_docstring(catalog.column_description(table, col).as_deref(), "  "))
                            .deprecated(col.deprecation.as_deref());
                        field.directives.push_str(&Self::constraint_directive(table, col));
                        field
                    }
                    FieldKind::Relation(rel) => {
                        let target = type_of(&rel.target_schema, &rel.target_table);
                        let args = catalog
                            .find_table(&rel.target_schema, &rel.target_table)
                            .and_then(Self::as_of_arg)
                            .map(|a| format!("({a})"))
                            .unwrap_or_default();
                        let decl = if rel.is_many { format!("{args}: [{target}!]!") } else { format!("{args}: {target}") };
                        let doc = if rel.history {
                            "  \"\"\"Every version of this row (the rows sharing its key apart from the period), oldest first.\"\"\"\n".to_string()
                        } else {
                            String::new()
                        };
                        let deprecation = catalog.find_table(&rel.target_schema, &rel.target_table).and_then(|t| t.deprecation.as_deref());
                        SdlField::new(name, decl).with_doc(doc).deprecated(deprecation)
                    }
                    FieldKind::Connection(rel) => {
                        let target = catalog.find_table(&rel.target_schema, &rel.target_table)?;
                        let args = Self::format_connection_args(target);
                        SdlField::new(name, format!("({args}): {}Connection!", target.type_name)).deprecated(target.deprecation.as_deref())
                    }
                    FieldKind::Tree(info) => {
                        let (what, order) = if info.ancestors {
                            ("above this row: its parent, the parent's parent, and so on", "nearest first")
                        } else {
                            ("below this row: its children, their children, and so on", "nearest first, then by key")
                        };
                        let how = match &info.link {
                            TreeLink::ForeignKey(_) => "Rows met again (a cycle) are left out.".to_string(),
                            TreeLink::Path(column) => {
                                let field = table.get_column(column).map(|c| c.field_name.clone()).unwrap_or_default();
                                format!("By the `{field}` path.")
                            }
                        };
                        let doc = format!("  \"\"\"The rows {what} ({order}). {how}\"\"\"\n");
                        let t = &table.type_name;
                        SdlField::new(
                            name,
                            format!("(\"\"\"Rows at most this many levels away.\"\"\" maxDepth: Int, filter: {t}Filter, orderBy: [{t}OrderBy!]): [{t}!]!"),
                        )
                        .with_doc(doc)
                    }
                    FieldKind::RowVersion => SdlField::new(name, ": String").with_doc(
                        "  \"\"\"Version of the row, changed by every write of it: pass it as `expectedVersion` to update or delete the row only if it did not change since. Null for a row that is not stored in the table (e.g. one a function built).\"\"\"\n".to_string(),
                    ),
                    FieldKind::ArcTarget => {
                        let arc = catalog.exclusive_arcs.iter().find(|a| a.table_name == table.name)?;
                        SdlField::new(name, format!(": {}!", arc.union_name))
                    }
                    FieldKind::Function(i) => {
                        let func = &catalog.functions[*i];
                        if !catalog.function_is_exposed(func) {
                            return None;
                        }
                        let decl = if func.row_argument {
                            Self::function_field_type(catalog, func, false)
                        } else {
                            format!("{}: [{}!]!", Self::function_args(catalog, func, true), func.row_type_name())
                        };
                        SdlField::new(name, decl)
                            .with_doc(Self::format_docstring(func.description.as_deref(), "  "))
                            .deprecated(func.deprecation.as_deref())
                    }
                })
            })
            .collect()
    }

    /// GraphQL arguments of a function field: `(a: Int!, b: String)`, empty without arguments.
    /// A mounted function takes the arguments not filled from the parent row. Arguments
    /// with a default are optional; key-like ones (`uuid`, `*_id`) are `ID`s, which also
    /// take Relay IDs.
    fn function_args(catalog: &DatabaseCatalog, func: &FunctionMetadata, mounted: bool) -> String {
        let args = Self::function_arg_list(catalog, func, mounted);
        if args.is_empty() {
            String::new()
        } else {
            format!("({})", args.join(", "))
        }
    }

    fn function_arg_list(
        catalog: &DatabaseCatalog,
        func: &FunctionMetadata,
        mounted: bool,
    ) -> Vec<String> {
        // A computed column's row is the parent's
        func.arguments
            .iter()
            .skip(usize::from(func.row_argument))
            .filter(|a| !(mounted && func.parent_column_for(&a.name).is_some()))
            .map(|a: &FunctionArgument| {
                let ty = if a.is_key_like() {
                    "ID".to_string()
                } else {
                    catalog.graphql_type(&a.pg_type, pg_type_enum_key(&a.pg_type).as_deref(), true)
                };
                let non_null = if a.has_default { "" } else { "!" };
                format!("{}: {}{}", a.graphql_name(), ty, non_null)
            })
            .collect()
    }

    /// Arguments and type of a root field backed by a function. In a query, rows of a table
    /// returned as a set are a connection: the function's arguments, then the connection's
    /// (except any a function argument's name takes).
    fn function_field_sdl(
        catalog: &DatabaseCatalog,
        func: &FunctionMetadata,
        mutation: bool,
    ) -> String {
        format!(
            "{}{}",
            func.field_name,
            Self::function_field_type(catalog, func, mutation)
        )
    }

    /// `(args): Type` of a field backed by a function (see `function_field_sdl`).
    fn function_field_type(
        catalog: &DatabaseCatalog,
        func: &FunctionMetadata,
        mutation: bool,
    ) -> String {
        let list = |item: String, non_null_items: bool| {
            if !func.returns_set {
                item
            } else if non_null_items {
                format!("[{item}!]!")
            } else {
                format!("[{item}]!")
            }
        };
        let mut args = Self::function_arg_list(catalog, func, false);
        let ty = match &func.returns {
            FunctionReturn::Table { schema, table } => {
                let Some(target) = catalog.find_table(schema, table) else {
                    return String::new();
                };
                if func.returns_set && !mutation {
                    let own: HashSet<String> =
                        func.arguments.iter().map(|a| a.graphql_name()).collect();
                    let conn = Self::format_connection_args(target);
                    args.extend(
                        conn.split(", ")
                            .filter(|a| !own.contains(a.split(':').next().unwrap_or("")))
                            .map(str::to_string),
                    );
                    format!("{}Connection!", target.type_name)
                } else {
                    list(target.type_name.clone(), true)
                }
            }
            FunctionReturn::Record => list(func.row_type_name(), true),
            FunctionReturn::Value(pg_type) => list(
                catalog.graphql_type(pg_type, pg_type_enum_key(pg_type).as_deref(), false),
                false,
            ),
            FunctionReturn::Void => "Boolean".to_string(),
        };
        let args = if args.is_empty() {
            String::new()
        } else {
            format!("({})", args.join(", "))
        };
        format!("{args}: {ty}")
    }

    /// Names of the fields of `table`'s own GraphQL type.
    pub fn type_field_names(catalog: &DatabaseCatalog, table: &TableMetadata) -> HashSet<String> {
        catalog
            .object_fields(&table.schema, &table.name)
            .iter()
            .map(|f| f.name.clone())
            .collect()
    }

    /// Fields of a CTI child table's concrete type: every field of each parent interface
    /// (first parent wins on shared names), then the child's own remaining fields.
    pub fn cti_child_fields(catalog: &DatabaseCatalog, table: &TableMetadata) -> Vec<SdlField> {
        let mut fields: Vec<SdlField> = Vec::new();
        for (cti, _) in catalog.cti_parents_of(&table.schema, &table.name) {
            if let Some(parent) = catalog.find_table(&cti.schema, &cti.parent_table) {
                for f in Self::table_object_fields(catalog, parent) {
                    if !fields.iter().any(|e| e.name == f.name) {
                        fields.push(f);
                    }
                }
            }
        }
        for f in Self::table_object_fields(catalog, table) {
            if !fields.iter().any(|e| e.name == f.name) {
                fields.push(f);
            }
        }
        fields
    }

    /// A child table under several CTI parents implements every parent interface, which is
    /// only valid when shared field names have compatible types. Detaches the child from
    /// parents it cannot implement (and drops hierarchies left without children).
    pub fn prune_incompatible_cti_children(catalog: &mut DatabaseCatalog) {
        let children: Vec<(String, String)> = catalog
            .cti
            .iter()
            .flat_map(|c| {
                c.children
                    .iter()
                    .map(|ch| (ch.schema.clone(), ch.table.clone()))
            })
            .collect();
        for (schema, table) in children {
            while let Some(meta) = catalog.find_table(&schema, &table) {
                let merged = Self::cti_child_fields(catalog, meta);
                let incompatible = catalog
                    .cti_parents_of(&schema, &table)
                    .into_iter()
                    .skip(1)
                    .find_map(|(cti, _)| {
                        let parent = catalog.find_table(&cti.schema, &cti.parent_table)?;
                        let ok = Self::table_object_fields(catalog, parent).iter().all(|f| {
                            merged
                                .iter()
                                .find(|m| m.name == f.name)
                                .is_some_and(|m| m.implements(f))
                        });
                        (!ok).then(|| (cti.schema.clone(), cti.parent_table.clone()))
                    });
                let Some((p_schema, p_table)) = incompatible else {
                    break;
                };
                if let Some(cti) = catalog
                    .cti
                    .iter_mut()
                    .find(|c| c.schema == p_schema && c.parent_table == p_table)
                {
                    cti.children
                        .retain(|ch| !(ch.schema == schema && ch.table == table));
                }
            }
        }
        catalog.cti.retain(|c| !c.children.is_empty());
        catalog.invalidate_index();
    }

    /// Object type(s) for a table:
    /// - polymorphic table: `interface <Base>` plus its concrete STI variant / fallback types
    /// - CTI child table: `type <Child> implements <Parent>...` with the parents' fields
    /// - otherwise a plain object type
    fn object_type_sdl(catalog: &DatabaseCatalog, table: &TableMetadata) -> String {
        let mut sdl = String::new();
        let type_name = &table.type_name;
        let doc = Self::format_docstring(catalog.table_description(table).as_deref(), "");
        let has_id =
            |fields: &[SdlField]| fields.iter().any(|f| f.name == "id" && f.decl == ": ID!");
        let implements = |node: bool, interfaces: &[String]| {
            let mut list: Vec<String> = Vec::new();
            if node {
                list.push("Node".to_string());
            }
            list.extend(interfaces.iter().cloned());
            if list.is_empty() {
                String::new()
            } else {
                format!(" implements {}", list.join(" & "))
            }
        };
        let fed_directives = if catalog.federation_enabled {
            let mut d = String::new();
            if let Some(key) = table.federation_key_fields() {
                d.push_str(&format!(" @key(fields: \"{}\")", key));
            }
            if table.federation_shareable {
                d.push_str(" @shareable");
            }
            d
        } else {
            String::new()
        };

        if let Some(poly) = catalog.find_polymorphism(&table.schema, &table.name) {
            let fields = Self::table_object_fields(catalog, table);
            let node = has_id(&fields) && table.implements_node();
            let body = SdlField::render(&fields);
            sdl.push_str(&doc);
            sdl.push_str(&format!(
                "interface {}{}{} {{\n{}}}\n\n",
                type_name,
                implements(node, &[]),
                fed_directives,
                body
            ));

            let iface = [type_name.clone()];
            if let Polymorphism::Sti(sti) = poly {
                for v in &sti.variants {
                    sdl.push_str(&format!(
                        "\"\"\"A `{}` whose `{}` is `{}`.\"\"\"\n",
                        type_name,
                        sti.discriminator_column,
                        v.discriminator_value.replace('"', "'").replace('\\', "")
                    ));
                    sdl.push_str(&format!(
                        "type {}{}{} {{\n{}}}\n\n",
                        v.type_name,
                        implements(node, &iface),
                        fed_directives,
                        body
                    ));
                }
            }
            if let Some(fallback) = poly.fallback_type_name() {
                sdl.push_str(&format!(
                    "\"\"\"A `{}` that is none of its more specific types.\"\"\"\n",
                    type_name
                ));
                sdl.push_str(&format!(
                    "type {}{}{} {{\n{}}}\n\n",
                    fallback,
                    implements(node, &iface),
                    fed_directives,
                    body
                ));
            }
            return sdl;
        }

        let parents = catalog.cti_parents_of(&table.schema, &table.name);
        let (fields, interfaces): (Vec<SdlField>, Vec<String>) = if parents.is_empty() {
            (Self::table_object_fields(catalog, table), Vec::new())
        } else {
            let interfaces = parents
                .iter()
                .map(|(c, _)| catalog.type_name_of(&c.schema, &c.parent_table))
                .collect();
            (Self::cti_child_fields(catalog, table), interfaces)
        };
        let node = has_id(&fields);
        sdl.push_str(&doc);
        sdl.push_str(&format!(
            "type {}{}{} {{\n{}}}\n\n",
            type_name,
            implements(node, &interfaces),
            fed_directives,
            SdlField::render(&fields)
        ));
        sdl
    }

    pub fn table_to_type_name(table_name: &str) -> String {
        crate::catalog::table_to_type_name(table_name)
    }

    pub fn to_pascal_case(s: &str) -> String {
        crate::catalog::to_pascal_case(s)
    }

    pub fn to_camel_case(s: &str) -> String {
        to_camel_case(s)
    }

    pub fn format_docstring(description: Option<&str>, indent: &str) -> String {
        match description {
            Some(desc) if !desc.trim().is_empty() => {
                let trimmed = desc.trim();
                let mut sanitized = trimmed.replace("\"\"\"", "\\\"\\\"\\\"");
                // In GraphQL SDL block strings ("""..."""), if the content ends with a double-quote,
                // appending """ directly produces """" which causes lexer ambiguity.
                // Appending a space ensures clean termination.
                if sanitized.ends_with('"') {
                    sanitized.push(' ');
                }
                format!("{}\"\"\"{}\"\"\"\n", indent, sanitized)
            }
            _ => String::new(),
        }
    }

    /// Filter input type of a column, if it can be filtered.
    pub fn column_filter_type(catalog: &DatabaseCatalog, col: &ColumnMetadata) -> Option<String> {
        if let Some(e) = catalog.column_enum(col) {
            let list = if col.pg_type.is_array() { "List" } else { "" };
            return Some(format!("{}{list}Filter", e.type_name));
        }
        if let Some(range_input) = col.pg_type.range_filter_input_name() {
            return Some(range_input);
        }
        let name = match &col.pg_type {
            PgType::Uuid => "UUIDFilter",
            PgType::Text | PgType::Varchar | PgType::Char | PgType::Citext => "StringFilter",
            PgType::Int2 | PgType::Int4 => "IntFilter",
            PgType::Int8 => "BigIntFilter",
            PgType::Float4 | PgType::Float8 => "FloatFilter",
            PgType::Numeric => "DecimalFilter",
            PgType::Bool => "BooleanFilter",
            PgType::Json | PgType::Jsonb => "JSONFilter",
            PgType::Timestamp => "LocalDatetimeFilter",
            PgType::TimestampTz => "DatetimeFilter",
            PgType::Date => "DateFilter",
            PgType::Time => "TimeFilter",
            PgType::TimeTz => "TimeTzFilter",
            PgType::Interval => "IntervalFilter",
            PgType::Money => "DecimalFilter",
            PgType::Inet => "InternetAddressFilter",
            PgType::Cidr => "CidrAddressFilter",
            PgType::MacAddr | PgType::MacAddr8 => "MacAddressFilter",
            PgType::Ltree => "LTreeFilter",
            PgType::TsVector => "FullTextFilter",
            PgType::Geometry | PgType::Geography => "GeoJSONFilter",
            PgType::Multirange(range) => {
                return Some(format!("{}MultirangeFilter", range.range_element()?.1))
            }
            PgType::Bytea | PgType::Vector => return None,
            // Arrays of types with a native cast compare natively; others cannot be filtered
            PgType::Array(elem) if col.pg_type.sql_cast().is_some() && !elem.is_array() => {
                return Some(format!("{}ListFilter", elem.to_graphql_scalar()));
            }
            PgType::Array(_) | PgType::Composite(_) => return None,
            _ => "StringFilter",
        };
        Some(name.to_string())
    }

    /// Decodes a Relay Global ID (base64("<TypeName>:<raw_pk>")) into (type_name, raw_pk).
    /// Returns None if the string is not valid base64 or doesn't contain ':'.
    pub fn decode_global_id(global_id: &str) -> Option<(String, String)> {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let cleaned: String = global_id.chars().filter(|c| !c.is_whitespace()).collect();
        let decoded = STANDARD.decode(&cleaned).ok()?;
        let s = String::from_utf8(decoded).ok()?;
        let colon = s.find(':')?;
        Some((s[..colon].to_string(), s[colon + 1..].to_string()))
    }

    /// Encodes a Relay Global ID: base64("<TypeName>:<raw_pk>")
    pub fn encode_global_id(type_name: &str, raw_pk: &str) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine};
        STANDARD.encode(format!("{}:{}", type_name, raw_pk))
    }

    /// Columns of `table`'s `<Type>Condition` input: exposed columns compared by equality
    /// (composite values are not).
    fn condition_columns(table: &TableMetadata) -> impl Iterator<Item = &ColumnMetadata> {
        let composite = |t: &PgType| matches!(t, PgType::Composite(_));
        table.visible_columns().filter(move |c| match &c.pg_type {
            PgType::Array(elem) => !composite(elem),
            other => !composite(other),
        })
    }

    /// Whether `table` gets a `<Type>Condition` input (it needs at least one field).
    fn has_condition(table: &TableMetadata) -> bool {
        Self::condition_columns(table).next().is_some()
    }

    /// `asOf` argument of fields reading rows of a table with a temporal key.
    fn as_of_arg(table: &TableMetadata) -> Option<String> {
        let scalar = table.as_of_scalar()?;
        let period = table
            .get_column(&table.period_key()?.period)?
            .field_name
            .clone();
        Some(format!("\"\"\"Only the versions valid at this point: whose `{period}` contains it.\"\"\" asOf: {scalar}"))
    }

    /// ` @constraint(minimum: 0, maxLength: 20, …)`: what the table's `CHECK` constraints and the
    /// column's `varchar(n)` say of its values, as the same keywords the OpenAPI document uses
    /// (JSON Schema). Empty when there are none.
    fn constraint_directive(table: &TableMetadata, col: &ColumnMetadata) -> String {
        const KEYWORDS: [&str; 10] = [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
            "minLength",
            "maxLength",
            "pattern",
            "minItems",
            "maxItems",
        ];
        let mut keywords = crate::openapi::constraints::keywords(table, col);
        if let Some(limit) = col.limits.max_length {
            // A `varchar(20)` limit and a `CHECK` on the length: the stricter one holds
            let tighter = keywords
                .get("maxLength")
                .and_then(|v| v.as_u64())
                .is_none_or(|check| u64::from(limit) < check);
            if tighter {
                keywords.insert("maxLength".to_string(), serde_json::json!(limit));
            }
        }
        let args: Vec<String> = KEYWORDS
            .iter()
            .filter_map(|k| keywords.get(*k).map(|v| format!("{k}: {v}")))
            .collect();
        if args.is_empty() {
            String::new()
        } else {
            format!(" @constraint({})", args.join(", "))
        }
    }

    /// Arguments of a connection field over rows of `table`.
    pub fn format_connection_args(table: &TableMetadata) -> String {
        let type_name = &table.type_name;
        let mut args =
            "first: Int, after: String, last: Int, before: String, offset: Int".to_string();
        if let Some(as_of) = Self::as_of_arg(table) {
            args.push_str(&format!(", {as_of}"));
        }
        if Self::has_condition(table) {
            args.push_str(&format!(", condition: {type_name}Condition"));
        }
        args.push_str(&format!(
            ", filter: {type_name}Filter, orderBy: [{type_name}OrderBy!]"
        ));
        args
    }

    /// Arguments of search root field `method` over rows of `table`: the method's own, then
    /// `first` and the conditions and filter the matches must pass.
    fn search_args(
        catalog: &DatabaseCatalog,
        table: &TableMetadata,
        method: SearchMethod,
    ) -> String {
        let type_name = &table.type_name;
        let limits = catalog.search_limits;
        let mut args = vec![match method {
            SearchMethod::Fuzzy => format!(
                "\"\"\"Text to match by trigram word similarity: how well it matches some part of the value.\"\"\" query: String!, \
                 \"\"\"Column to search (the first indexed one by default).\"\"\" field: {type_name}FuzzyField, \
                 \"\"\"Least word similarity (0 to 1) of a match; by default the database setting `pg_trgm.word_similarity_threshold` (0.6).\"\"\" minSimilarity: Float"
            ),
            SearchMethod::Semantic => format!(
                "\"\"\"Embedding to find the nearest rows to.\"\"\" vector: [Float!]!, \
                 \"\"\"Column to search (the first indexed one by default).\"\"\" field: {type_name}VectorField, \
                 \"\"\"Least similarity of a match.\"\"\" minScore: Float"
            ),
            SearchMethod::Bm25 => format!(
                "\"\"\"Full-text query.\"\"\" query: String!, \
                 \"\"\"Index to search (the first one by default).\"\"\" field: {type_name}Bm25Field"
            ),
            SearchMethod::FullText => format!(
                "\"\"\"Web-search-style query: words (stemmed), `\\\"quoted phrases\\\"`, `or` (`OR` for PGroonga indexes), `-excluded`.\"\"\" query: String!, \
                 \"\"\"Document to search (the first indexed one by default).\"\"\" field: {type_name}FullTextField"
            ),
            SearchMethod::Nearest => format!(
                "\"\"\"The geometry to find the nearest rows to.\"\"\" near: GeoJSON!, \
                 \"\"\"Column to search (the first indexed one by default).\"\"\" field: {type_name}SpatialField, \
                 \"\"\"Farthest distance of a match (meters for geographies).\"\"\" maxDistance: Float"
            ),
            SearchMethod::Hybrid => "\"\"\"Text of the lexical ranking.\"\"\" query: String!, \
                 \"\"\"Embedding of the vector ranking.\"\"\" vector: [Float!]!, \
                 \"\"\"Weight of the lexical ranking (0.5).\"\"\" lexicalWeight: Float, \
                 \"\"\"Weight of the vector ranking (0.5).\"\"\" vectorWeight: Float, \
                 \"\"\"Reciprocal rank fusion constant (60).\"\"\" rrfK: Int"
                .to_string(),
        }];
        args.push(format!(
            "\"\"\"Number of hits: {} by default, at most {}.\"\"\" first: Int",
            limits.default, limits.max
        ));
        if Self::has_condition(table) {
            args.push(format!("condition: {type_name}Condition"));
        }
        args.push(format!("filter: {type_name}Filter"));
        args.join(", ")
    }

    /// Types of `groupedAggregates`: `<Type>GroupBy`, `<Type>AggregateGroup` and the
    /// `having` inputs. `numeric`: the table has numeric columns (sums, averages, and the
    /// aggregates `having` compares).
    fn grouped_aggregates_sdl(table: &TableMetadata, numeric: bool) -> String {
        let type_name = &table.type_name;
        let mut sdl = format!(
            "\"\"\"What `{type_name}` rows are grouped by: a column, or a date or timestamp truncated to a unit of time.\"\"\"\nenum {type_name}GroupBy {{\n"
        );
        for (value, _, _) in table.group_by_values() {
            sdl.push_str(&format!("  {value}\n"));
        }
        sdl.push_str("}\n\n");
        sdl.push_str(&format!(
            "\"\"\"Aggregates of the `{type_name}` rows sharing the same `groupBy` values.\"\"\"\ntype {type_name}AggregateGroup {{\n  \
             \"\"\"The group's values, in `groupBy` order.\"\"\"\n  keys: [JSON]!\n  count: Int!\n"
        ));
        sdl.push_str(&Self::aggregate_fields_sdl(type_name, numeric));
        sdl.push_str("}\n\n");
        sdl.push_str(&format!("\"\"\"Conditions groups must meet, all of them.\"\"\"\ninput {type_name}HavingInput {{\n  count: IntFilter\n"));
        if numeric {
            for kind in ["sum", "avg", "min", "max"] {
                sdl.push_str(&format!("  {kind}: {type_name}HavingNumbersInput\n"));
            }
        }
        sdl.push_str("}\n\n");
        if numeric {
            sdl.push_str(&format!("input {type_name}HavingNumbersInput {{\n"));
            for col in table.visible_columns().filter(|c| c.pg_type.is_numeric()) {
                sdl.push_str(&format!("  {}: FloatFilter\n", col.field_name));
            }
            sdl.push_str("}\n\n");
        }
        sdl
    }

    /// `<Type>ToManyFilter`: conditions on the rows of `table` related to a row.
    fn to_many_filter_sdl(table: &TableMetadata) -> String {
        let t = &table.type_name;
        let mut sdl = format!(
            "\"\"\"Conditions on the `{t}` rows related to a row: all of them must hold.\"\"\"\ninput {t}ToManyFilter {{\n  \
             \"\"\"Some related row matches.\"\"\"\n  some: {t}Filter\n  \
             \"\"\"Every related row matches (also true without related rows).\"\"\"\n  every: {t}Filter\n  \
             \"\"\"No related row matches.\"\"\"\n  none: {t}Filter\n  \
             \"\"\"Number of related rows.\"\"\"\n  count: IntFilter\n"
        );
        if table.visible_columns().any(|c| c.pg_type.is_numeric()) {
            for kind in ["sum", "avg", "min", "max"] {
                sdl.push_str(&format!("  \"\"\"The {kind} of a column over the related rows (null without related rows, except counts).\"\"\"\n  {kind}: {t}HavingNumbersInput\n"));
            }
        }
        sdl.push_str("}\n\n");
        sdl
    }

    /// Fields of `<Type>Aggregates` and `<Type>AggregateGroup` objects. `numeric`: the table
    /// has numeric columns (sums, averages, statistics).
    fn aggregate_fields_sdl(type_name: &str, numeric: bool) -> String {
        let mut sdl = format!(
            "  min: {type_name}MinAggregate\n  max: {type_name}MaxAggregate\n  \
             \"\"\"Number of distinct non-null values.\"\"\"\n  distinctCount: {type_name}DistinctCountAggregate\n"
        );
        if numeric {
            sdl.push_str(&format!(
                "  sum: {type_name}SumAggregate\n  avg: {type_name}AvgAggregate\n  \
                 stddevSample: {type_name}StatisticAggregate\n  stddevPopulation: {type_name}StatisticAggregate\n  \
                 varianceSample: {type_name}StatisticAggregate\n  variancePopulation: {type_name}StatisticAggregate\n  \
                 \"\"\"The middle value (continuous: interpolated between the two middle values).\"\"\"\n  median: {type_name}StatisticAggregate\n  \
                 \"\"\"The value below which this fraction of the values falls (continuous percentile).\"\"\"\n  \
                 percentile(\"\"\"Between 0 and 1, e.g. 0.95.\"\"\" fraction: Float!): {type_name}StatisticAggregate\n"
            ));
        }
        sdl
    }

    /// `enum <name>` of the columns searchable through `indexes`.
    fn search_field_enum(
        catalog: &DatabaseCatalog,
        name: &str,
        indexes: &[&SearchIndexMetadata],
    ) -> String {
        let values: BTreeSet<String> = indexes
            .iter()
            .map(|i| catalog.search_field_value(i))
            .collect();
        let values: String = values.iter().map(|v| format!("  {v}\n")).collect();
        format!("enum {name} {{\n{values}}}\n\n")
    }

    /// Arguments addressing one row by key: one per key column, typed as the column; a row of
    /// a table with a temporal key by its entity and a point in time.
    fn key_args(catalog: &DatabaseCatalog, table: &TableMetadata) -> Option<String> {
        let key = table.key_columns()?;
        if let (Some(period), Some(as_of)) = (table.period_key(), Self::as_of_arg(table)) {
            if table.primary_key.contains(&period.period) {
                let mut args: Vec<String> = key
                    .iter()
                    .filter(|c| c.name != period.period)
                    .map(|c| format!("{}: {}!", c.field_name, catalog.column_type(c, true)))
                    .collect();
                args.push(as_of);
                return Some(args.join(", "));
            }
        }
        Some(
            key.iter()
                .map(|c| format!("{}: {}!", c.field_name, catalog.column_type(c, true)))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// Generate GraphQL SDL for all tables in the catalog including Relay Connections.
    pub fn generate_sdl(catalog: &DatabaseCatalog) -> String {
        Self::generate_sdl_internal(catalog, false)
    }

    /// Generate Apollo Federation Subgraph SDL (for `_service { sdl }`)
    pub fn generate_federation_subgraph_sdl(catalog: &DatabaseCatalog) -> String {
        Self::generate_sdl_internal(catalog, true)
    }

    fn generate_sdl_internal(catalog: &DatabaseCatalog, for_subgraph_service: bool) -> String {
        let mut sdl = String::new();

        if for_subgraph_service && catalog.federation_enabled {
            let fed_version = catalog.federation_version.as_deref().unwrap_or("v2.3");
            sdl.push_str(&format!(
                "extend schema\n  @link(url: \"https://specs.apollo.dev/federation/{fed_version}\", import: [\"@key\", \"@shareable\"])\n\n"
            ));
        }

        let mut entity_type_names = Vec::new();
        if catalog.federation_enabled {
            for table in &catalog.tables {
                if table.is_federated_entity() {
                    if let Some(poly) = catalog.find_polymorphism(&table.schema, &table.name) {
                        match poly {
                            Polymorphism::Sti(sti) => {
                                for v in &sti.variants {
                                    entity_type_names.push(v.type_name.clone());
                                }
                            }
                            Polymorphism::Cti(_) => {}
                        }
                    } else {
                        entity_type_names.push(table.type_name.clone());
                    }
                }
            }
            entity_type_names.sort();
            entity_type_names.dedup();
        }

        if !for_subgraph_service && catalog.federation_enabled {
            sdl.push_str("scalar _Any\n");
            sdl.push_str("scalar _FieldSet\n\n");
            sdl.push_str("directive @key(fields: _FieldSet!, resolvable: Boolean = true) repeatable on OBJECT | INTERFACE\n");
            sdl.push_str("directive @shareable repeatable on OBJECT | FIELD_DEFINITION\n\n");
            sdl.push_str("type _Service {\n  sdl: String!\n}\n\n");
            if !entity_type_names.is_empty() {
                sdl.push_str(&format!(
                    "union _Entity = {}\n\n",
                    entity_type_names.join(" | ")
                ));
            }
        }

        for (name, description) in CUSTOM_SCALARS {
            sdl.push_str(&format!("\"\"\"{description}\"\"\"\nscalar {name}\n\n"));
        }
        for d in &catalog.domains {
            let base = catalog.graphql_type(&d.base, None, false);
            let about = format!("Values of domain `{}`, as `{base}` values.", d.config_name);
            let doc = match &d.description {
                Some(desc) if !desc.trim().is_empty() => format!("{}\n\n{about}", desc.trim()),
                _ => about,
            };
            sdl.push_str(&Self::format_docstring(Some(&doc), ""));
            sdl.push_str(&format!("scalar {}\n\n", d.type_name));
        }

        // Relay Node interface — the foundation for global object identification
        sdl.push_str("interface Node {\n");
        sdl.push_str("  id: ID!\n");
        sdl.push_str("}\n\n");

        // Standard Relay PageInfo type
        sdl.push_str("type PageInfo {\n");
        sdl.push_str("  hasNextPage: Boolean!\n");
        sdl.push_str("  hasPreviousPage: Boolean!\n");
        sdl.push_str("  startCursor: String\n");
        sdl.push_str("  endCursor: String\n");
        sdl.push_str("}\n\n");

        sdl.push_str(concat!(
            "\"\"\"What the database enforces on a value, in the keywords of JSON Schema (and so of the OpenAPI document): ",
            "a `CHECK` on the column, or its `varchar(n)` length. Documentation for clients and code generators; ",
            "the database checks every write.\"\"\"\n",
            "directive @constraint(minimum: Float, maximum: Float, exclusiveMinimum: Float, exclusiveMaximum: Float, ",
            "multipleOf: Float, minLength: Int, maxLength: Int, pattern: String, minItems: Int, maxItems: Int) ",
            "on FIELD_DEFINITION | INPUT_FIELD_DEFINITION\n\n"
        ));
        sdl.push_str("directive @defer(label: String, if: Boolean = true) on FRAGMENT_SPREAD | INLINE_FRAGMENT\n\n");
        sdl.push_str("directive @stream(label: String, initialCount: Int = 0, if: Boolean = true) on FIELD\n\n");

        // Standard Filter Inputs
        sdl.push_str("input StringFilter {\n");
        sdl.push_str("  isNull: Boolean\n");
        sdl.push_str("  equalTo: String\n");
        sdl.push_str("  notEqualTo: String\n");
        sdl.push_str("  in: [String!]\n");
        sdl.push_str("  notIn: [String!]\n");
        sdl.push_str("  lessThan: String\n");
        sdl.push_str("  lessThanOrEqualTo: String\n");
        sdl.push_str("  greaterThan: String\n");
        sdl.push_str("  greaterThanOrEqualTo: String\n");
        sdl.push_str("  \"\"\"Contains the substring (`%` and `_` match literally).\"\"\"\n");
        sdl.push_str("  contains: String\n");
        sdl.push_str("  notContains: String\n");
        sdl.push_str("  startsWith: String\n");
        sdl.push_str("  notStartsWith: String\n");
        sdl.push_str("  endsWith: String\n");
        sdl.push_str("  notEndsWith: String\n");
        sdl.push_str("  \"\"\"SQL `LIKE` pattern (`%` and `_` are wildcards).\"\"\"\n");
        sdl.push_str("  like: String\n");
        sdl.push_str("  notLike: String\n");
        sdl.push_str("  ilike: String\n");
        sdl.push_str("  notIlike: String\n");
        sdl.push_str("  \"\"\"POSIX regular expression match (`~`).\"\"\"\n");
        sdl.push_str("  regex: String\n");
        sdl.push_str("  \"\"\"Case-insensitive POSIX regular expression match (`~*`).\"\"\"\n");
        sdl.push_str("  iregex: String\n");
        sdl.push_str(
            "  \"\"\"`IS DISTINCT FROM`: like `notEqualTo`, but a null is a value.\"\"\"\n",
        );
        sdl.push_str("  isDistinctFrom: String\n");
        sdl.push_str("  \"\"\"`LIKE` any or all of the patterns.\"\"\"\n");
        sdl.push_str("  likeAny: [String!]\n  likeAll: [String!]\n  ilikeAny: [String!]\n  ilikeAll: [String!]\n");
        sdl.push_str("  \"\"\"A comparison that holds against any or all of the values (`< ANY (...)`).\"\"\"\n");
        sdl.push_str("  lessThanAny: [String!]\n  lessThanAll: [String!]\n  lessThanOrEqualToAny: [String!]\n  lessThanOrEqualToAll: [String!]\n");
        sdl.push_str("  greaterThanAny: [String!]\n  greaterThanAll: [String!]\n  greaterThanOrEqualToAny: [String!]\n  greaterThanOrEqualToAll: [String!]\n");
        sdl.push_str("}\n\n");

        for (name, ty, ordered) in COMPARISON_FILTERS {
            sdl.push_str(&format!("input {name} {{\n"));
            sdl.push_str("  isNull: Boolean\n");
            sdl.push_str(&format!("  equalTo: {ty}\n  notEqualTo: {ty}\n"));
            sdl.push_str(&format!("  isDistinctFrom: {ty}\n"));
            if *ty != "Boolean" {
                sdl.push_str(&format!("  in: [{ty}!]\n  notIn: [{ty}!]\n"));
            }
            if *ordered {
                sdl.push_str(&format!(
                    "  lessThan: {ty}\n  lessThanOrEqualTo: {ty}\n  greaterThan: {ty}\n  greaterThanOrEqualTo: {ty}\n"
                ));
                for name in [
                    "lessThan",
                    "lessThanOrEqualTo",
                    "greaterThan",
                    "greaterThanOrEqualTo",
                ] {
                    sdl.push_str(&format!("  {name}Any: [{ty}!]\n  {name}All: [{ty}!]\n"));
                }
            }
            sdl.push_str("}\n\n");
        }

        if catalog
            .tables
            .iter()
            .flat_map(|t| t.visible_columns())
            .any(|c| c.pg_type == PgType::Ltree)
        {
            sdl.push_str(concat!(
                "\"\"\"Conditions on an `ltree` label path, e.g. `top.science.astronomy`.\"\"\"\n",
                "input LTreeFilter {\n  isNull: Boolean\n  equalTo: String\n  notEqualTo: String\n  in: [String!]\n  notIn: [String!]\n",
                "  lessThan: String\n  lessThanOrEqualTo: String\n  greaterThan: String\n  greaterThanOrEqualTo: String\n",
                "  \"\"\"Is this path or above it (a prefix of it).\"\"\"\n  ancestorOf: String\n",
                "  \"\"\"Is this path or below it.\"\"\"\n  descendantOf: String\n",
                "  \"\"\"Matches an `lquery` pattern, e.g. `*.science.*`.\"\"\"\n  matches: String\n",
                "  \"\"\"Matches an `ltxtquery` of labels, e.g. `science & !biology`.\"\"\"\n  matchesText: String\n",
                "}\n\n",
            ));
        }

        sdl.push_str("input JSONFilter {\n");
        sdl.push_str("  isNull: Boolean\n");
        sdl.push_str("  equalTo: JSON\n");
        sdl.push_str("  notEqualTo: JSON\n");
        sdl.push_str("  contains: JSON\n");
        sdl.push_str("  containedBy: JSON\n");
        sdl.push_str("  \"\"\"A JSON path predicate holds, e.g. `$.age > 21` (`@@`).\"\"\"\n  matchesPath: String\n");
        sdl.push_str("  \"\"\"A JSON path selects some item, e.g. `$.tags[*] ? (@ == \\\"urgent\\\")` (`@?`).\"\"\"\n  existsPath: String\n");
        sdl.push_str(
            "  \"\"\"Has this top-level key (or array string element).\"\"\"\n  hasKey: String\n",
        );
        sdl.push_str("  hasAnyKeys: [String!]\n");
        sdl.push_str("  hasAllKeys: [String!]\n");
        sdl.push_str("}\n\n");

        // Filters of types with operators of their own, when columns have them
        let used: BTreeSet<String> = catalog
            .tables
            .iter()
            .flat_map(|t| t.visible_columns())
            .filter_map(|c| Self::column_filter_type(catalog, c))
            .collect();
        if used.contains("FullTextFilter") {
            sdl.push_str(concat!(
                "\"\"\"Conditions on a full-text document (`tsvector`).\"\"\"\ninput FullTextFilter {\n  isNull: Boolean\n",
                "  \"\"\"Matches a web-search-style query: words, `\\\"quoted phrases\\\"`, `or`, `-excluded`.\"\"\"\n  matches: String\n}\n\n",
            ));
        }
        if used.contains("GeoJSONFilter") {
            sdl.push_str(concat!(
                "\"\"\"Spatial conditions on a geometry (index-assisted).\"\"\"\ninput GeoJSONFilter {\n  isNull: Boolean\n",
                "  \"\"\"Shares some point with this geometry.\"\"\"\n  intersects: GeoJSON\n",
                "  \"\"\"Lies within this geometry.\"\"\"\n  within: GeoJSON\n",
                "  \"\"\"Contains this geometry.\"\"\"\n  contains: GeoJSON\n",
                "  \"\"\"Lies within a distance of a geometry (meters for geographies, units of the coordinate system otherwise).\"\"\"\n  withinDistance: GeoJSONDistanceInput\n",
                "  \"\"\"Its bounding box intersects this box.\"\"\"\n  intersectsBox: BoundingBoxInput\n}\n\n",
                "input GeoJSONDistanceInput {\n  geometry: GeoJSON!\n  distance: Float!\n}\n\n",
                "\"\"\"A box between two corners, in a spatial reference system (WGS 84, 4326, by default).\"\"\"\n",
                "input BoundingBoxInput {\n  xmin: Float!\n  ymin: Float!\n  xmax: Float!\n  ymax: Float!\n  srid: Int\n}\n\n",
            ));
        }
        for scalar in RANGE_SCALARS {
            if used.contains(&format!("{scalar}MultirangeFilter")) {
                sdl.push_str(&format!(
                    "\"\"\"Conditions on a multirange of `{scalar}` values (a set of ranges).\"\"\"\ninput {scalar}MultirangeFilter {{\n  isNull: Boolean\n  \
                     equalTo: [{scalar}RangeInput!]\n  notEqualTo: [{scalar}RangeInput!]\n  \"\"\"Some range contains this value.\"\"\"\n  contains: {scalar}\n  \
                     \"\"\"Some range contains this whole range.\"\"\"\n  containsRange: {scalar}RangeInput\n  overlaps: {scalar}RangeInput\n  \
                     \"\"\"Every range lies within this range.\"\"\"\n  containedBy: {scalar}RangeInput\n  isEmpty: Boolean\n}}\n\n"
                ));
            }
        }

        // Array filters, for the element types in use
        let list_filters: BTreeSet<String> = catalog
            .tables
            .iter()
            .flat_map(|t| t.visible_columns())
            .filter_map(|c| {
                Self::column_filter_type(catalog, c).filter(|f| f.ends_with("ListFilter"))
            })
            .collect();
        for filter in &list_filters {
            let ty = filter.trim_end_matches("ListFilter");
            sdl.push_str(&format!("input {filter} {{\n"));
            sdl.push_str("  isNull: Boolean\n");
            sdl.push_str(&format!("  equalTo: [{ty}]\n  notEqualTo: [{ty}]\n"));
            sdl.push_str(&format!(
                "  \"\"\"Contains all of these elements.\"\"\"\n  contains: [{ty}]\n"
            ));
            sdl.push_str(&format!(
                "  \"\"\"Every element is one of these.\"\"\"\n  containedBy: [{ty}]\n"
            ));
            sdl.push_str(&format!(
                "  \"\"\"Shares an element with these.\"\"\"\n  overlaps: [{ty}]\n"
            ));
            sdl.push_str("}\n\n");
        }

        // Enum types and their filters
        let enum_filters: BTreeSet<String> = catalog
            .tables
            .iter()
            .flat_map(|t| t.visible_columns())
            .filter(|c| !c.pg_type.is_array())
            .filter_map(|c| catalog.column_enum(c).map(|e| e.type_name.clone()))
            .collect();
        for e in &catalog.enums {
            sdl.push_str(&Self::format_docstring(e.description.as_deref(), ""));
            sdl.push_str(&format!("enum {} {{\n", e.type_name));
            for v in &e.values {
                if v.label != v.name {
                    sdl.push_str(&format!(
                        "  \"\"\"`{}`\"\"\"\n",
                        v.label.replace('"', "'").replace('\\', "")
                    ));
                }
                sdl.push_str(&format!("  {}\n", v.name));
            }
            sdl.push_str("}\n\n");
            if enum_filters.contains(&e.type_name) {
                let ty = &e.type_name;
                sdl.push_str(&format!(
                    "input {ty}Filter {{\n  isNull: Boolean\n  equalTo: {ty}\n  notEqualTo: {ty}\n  in: [{ty}!]\n  notIn: [{ty}!]\n}}\n\n"
                ));
            }
        }

        // Composite types, as object and input types
        for c in &catalog.composites {
            for input in [false, true] {
                sdl.push_str(&Self::format_docstring(c.description.as_deref(), ""));
                let (keyword, name) = if input {
                    ("input", c.input_type_name())
                } else {
                    ("type", c.type_name.clone())
                };
                sdl.push_str(&format!("{keyword} {name} {{\n"));
                for a in &c.attributes {
                    sdl.push_str(&Self::format_docstring(a.description.as_deref(), "  "));
                    let ty = catalog.graphql_type(&a.pg_type, a.enum_key.as_deref(), input);
                    sdl.push_str(&format!("  {}: {}\n", a.field_name, ty));
                }
                sdl.push_str("}\n\n");
            }
        }

        // Range types: values, inputs and filters, per bound scalar
        for scalar in RANGE_SCALARS {
            sdl.push_str(&format!(
                "\"\"\"A range of `{scalar}` values. A missing bound is unbounded.\"\"\"\ntype {scalar}Range {{\n  \
                 start: {scalar}RangeBound\n  end: {scalar}RangeBound\n  \"\"\"Contains no value (both bounds are then null).\"\"\"\n  isEmpty: Boolean!\n}}\n\n\
                 type {scalar}RangeBound {{\n  value: {scalar}!\n  inclusive: Boolean!\n}}\n\n\
                 \"\"\"A range of `{scalar}` values: a missing bound is unbounded; `start` is inclusive and `end` exclusive unless `inclusive` says otherwise.\"\"\"\n\
                 input {scalar}RangeInput {{\n  start: {scalar}RangeBoundInput\n  end: {scalar}RangeBoundInput\n}}\n\n\
                 input {scalar}RangeBoundInput {{\n  value: {scalar}!\n  inclusive: Boolean\n}}\n\n\
                 input {scalar}RangeFilter {{\n  isNull: Boolean\n  equalTo: {scalar}RangeInput\n  notEqualTo: {scalar}RangeInput\n  \
                 \"\"\"Contains this value.\"\"\"\n  contains: {scalar}\n  \"\"\"Lies within this range.\"\"\"\n  containedBy: {scalar}RangeInput\n  \
                 overlaps: {scalar}RangeInput\n  strictlyLeftOf: {scalar}RangeInput\n  strictlyRightOf: {scalar}RangeInput\n  adjacentTo: {scalar}RangeInput\n}}\n\n"
            ));
        }

        // 1b. Generate Exclusive Arc GraphQL Unions (polymorphic targets contribute their
        // concrete types, since unions cannot contain interfaces)
        for arc in &catalog.exclusive_arcs {
            let schema = catalog
                .find_table_any(&arc.table_name)
                .map(|t| t.schema.as_str())
                .unwrap_or("public");
            let member_types = catalog.arc_member_types(arc, schema);
            sdl.push_str(&format!(
                "union {} = {}\n\n",
                arc.union_name,
                member_types.join(" | ")
            ));
        }

        // 1c. Row types of functions returning columns of their own
        for func in catalog.functions.iter().filter(|f| f.returns_record()) {
            sdl.push_str(&Self::format_docstring(func.description.as_deref(), ""));
            sdl.push_str(&format!("type {} {{\n", func.row_type_name()));
            for col in &func.return_columns {
                let ty = catalog.graphql_type(
                    &col.pg_type,
                    pg_type_enum_key(&col.pg_type).as_deref(),
                    false,
                );
                sdl.push_str(&format!("  {}: {}\n", to_camel_case(&col.name), ty));
            }
            sdl.push_str("}\n\n");
        }

        let mut query_fields = Vec::new();
        let mut connection_types_sdl = String::new();
        // Tables to-many relation filters lead to
        let mut to_many_filters: BTreeSet<(String, String)> = BTreeSet::new();

        for table in &catalog.tables {
            let type_name = &table.type_name;
            let columns: Vec<&ColumnMetadata> = table.visible_columns().collect();

            // 1. Generate Condition Input
            if Self::has_condition(table) {
                connection_types_sdl.push_str(&format!("input {}Condition {{\n", type_name));
                for col in Self::condition_columns(table) {
                    connection_types_sdl.push_str(&format!(
                        "  {}: {}\n",
                        col.field_name,
                        catalog.column_type(col, true)
                    ));
                }
                connection_types_sdl.push_str("}\n\n");
            }

            // 1b. Generate Filter Input for Advanced Boolean Filtering
            let filter_type_name = format!("{}Filter", type_name);
            connection_types_sdl.push_str(&format!("input {} {{\n", filter_type_name));
            connection_types_sdl.push_str(&format!("  and: [{}!]\n", filter_type_name));
            connection_types_sdl.push_str(&format!("  or: [{}!]\n", filter_type_name));
            connection_types_sdl.push_str(&format!("  not: {}\n", filter_type_name));
            let mut filter_fields: HashSet<String> =
                ["and", "or", "not"].iter().map(|s| s.to_string()).collect();
            for col in &columns {
                if let Some(f_type) = Self::column_filter_type(catalog, col) {
                    if filter_fields.insert(col.field_name.clone()) {
                        connection_types_sdl
                            .push_str(&format!("  {}: {}\n", col.field_name, f_type));
                    }
                }
            }
            // Columns of the subtypes of a table-per-type hierarchy: some subtype row matches
            for (field_name, members) in catalog.cti_columns(table) {
                if let Some(f_type) = Self::column_filter_type(catalog, members[0].1) {
                    if filter_fields.insert(field_name.clone()) {
                        connection_types_sdl.push_str(&format!("  {field_name}: {f_type}\n"));
                    }
                }
            }
            // Relations: the related row matches; `some`/`every`/`none` of the related rows, or
            // their aggregates
            for field in catalog.object_fields(&table.schema, &table.name) {
                if let FieldKind::Relation(rel) = &field.kind {
                    if filter_fields.insert(field.name.clone()) {
                        let target = catalog.type_name_of(&rel.target_schema, &rel.target_table);
                        let suffix = if rel.is_many {
                            "ToManyFilter"
                        } else {
                            "Filter"
                        };
                        connection_types_sdl
                            .push_str(&format!("  {}: {target}{suffix}\n", field.name));
                        if rel.is_many {
                            to_many_filters
                                .insert((rel.target_schema.clone(), rel.target_table.clone()));
                        }
                    }
                }
            }
            connection_types_sdl.push_str("}\n\n");

            // 2. Generate OrderBy Enum for this table
            let order_by_type_name = format!("{}OrderBy", type_name);
            connection_types_sdl.push_str(&format!("enum {} {{\n", order_by_type_name));
            connection_types_sdl.push_str("  NATURAL\n");
            connection_types_sdl.push_str("  PRIMARY_KEY_ASC\n");
            connection_types_sdl.push_str("  PRIMARY_KEY_DESC\n");
            for (name, target) in catalog.order_options(table) {
                let doc = match &target {
                    OrderTarget::Column(_) => String::new(),
                    OrderTarget::Related { rel, column } => {
                        let target = catalog.find_table(&rel.target_schema, &rel.target_table);
                        let field = target
                            .and_then(|t| t.get_column(column))
                            .map(|c| c.field_name.clone())
                            .unwrap_or_default();
                        let ty = target.map(|t| t.type_name.clone()).unwrap_or_default();
                        format!("  \"\"\"By the `{field}` of the related `{ty}` (rows without one last ascending).\"\"\"\n")
                    }
                    OrderTarget::Count(rel) => {
                        format!(
                            "  \"\"\"By the number of related `{}` rows.\"\"\"\n",
                            catalog.type_name_of(&rel.target_schema, &rel.target_table)
                        )
                    }
                    OrderTarget::Function(i) => {
                        format!(
                            "  \"\"\"By the value of the computed field `{}`.\"\"\"\n",
                            catalog.functions[*i].mounted_field_name
                        )
                    }
                };
                connection_types_sdl.push_str(&format!("{doc}  {name}_ASC\n{doc}  {name}_DESC\n"));
                // NULLs last ascending and first descending by default; the other placements
                if catalog.order_target_nullable(table, &target) {
                    connection_types_sdl.push_str(&format!(
                        "  \"\"\"Ascending, NULLs first.\"\"\"\n  {name}_ASC_NULLS_FIRST\n  \"\"\"Descending, NULLs last.\"\"\"\n  {name}_DESC_NULLS_LAST\n"
                    ));
                }
            }
            connection_types_sdl.push_str("}\n\n");

            // 2b. Generate Aggregates Types for this table
            let orderable_cols: Vec<_> = columns
                .iter()
                .filter(|c| c.pg_type.is_orderable())
                .collect();
            let numeric_cols: Vec<_> = columns.iter().filter(|c| c.pg_type.is_numeric()).collect();
            let has_aggregates = !orderable_cols.is_empty();

            if has_aggregates {
                for kind in ["Min", "Max"] {
                    connection_types_sdl.push_str(&format!("type {type_name}{kind}Aggregate {{\n"));
                    for col in &orderable_cols {
                        connection_types_sdl.push_str(&format!(
                            "  {}: {}\n",
                            col.field_name,
                            catalog.column_type(col, false)
                        ));
                    }
                    connection_types_sdl.push_str("}\n\n");
                }
                if !numeric_cols.is_empty() {
                    for kind in ["Sum", "Avg"] {
                        connection_types_sdl
                            .push_str(&format!("type {type_name}{kind}Aggregate {{\n"));
                        for col in &numeric_cols {
                            connection_types_sdl
                                .push_str(&format!("  {}: Float\n", col.field_name));
                        }
                        connection_types_sdl.push_str("}\n\n");
                    }
                }

                connection_types_sdl
                    .push_str(&format!("type {type_name}DistinctCountAggregate {{\n"));
                for col in &orderable_cols {
                    connection_types_sdl.push_str(&format!("  {}: Int\n", col.field_name));
                }
                connection_types_sdl.push_str("}\n\n");
                if !numeric_cols.is_empty() {
                    connection_types_sdl
                        .push_str(&format!("type {type_name}StatisticAggregate {{\n"));
                    for col in &numeric_cols {
                        connection_types_sdl.push_str(&format!("  {}: Float\n", col.field_name));
                    }
                    connection_types_sdl.push_str("}\n\n");
                }

                connection_types_sdl.push_str(&format!("type {type_name}Aggregates {{\n"));
                connection_types_sdl.push_str(&Self::aggregate_fields_sdl(
                    type_name,
                    !numeric_cols.is_empty(),
                ));
                connection_types_sdl.push_str("}\n\n");
                connection_types_sdl.push_str(&Self::grouped_aggregates_sdl(
                    table,
                    !numeric_cols.is_empty(),
                ));
            }

            // 2c. Search field enums and result types, from the detected indexes
            let has_fuzzy = !catalog
                .fuzzy_indexes_for_table(&table.schema, &table.name)
                .is_empty();
            let has_vector = !catalog
                .vector_indexes_for_table(&table.schema, &table.name)
                .is_empty();
            let has_bm25 = !catalog
                .bm25_indexes_for_table(&table.schema, &table.name)
                .is_empty();

            if has_fuzzy {
                let fuzzy_field_enum = format!("{}FuzzyField", type_name);
                let values = catalog.fuzzy_indexes_for_table(&table.schema, &table.name);
                connection_types_sdl.push_str(&Self::search_field_enum(
                    catalog,
                    &fuzzy_field_enum,
                    &values,
                ));
            }
            if has_vector {
                let vector_field_enum = format!("{}VectorField", type_name);
                let values = catalog.vector_indexes_for_table(&table.schema, &table.name);
                connection_types_sdl.push_str(&Self::search_field_enum(
                    catalog,
                    &vector_field_enum,
                    &values,
                ));
            }
            if has_bm25 {
                let bm25_field_enum = format!("{}Bm25Field", type_name);
                let values = catalog.bm25_indexes_for_table(&table.schema, &table.name);
                connection_types_sdl.push_str(&Self::search_field_enum(
                    catalog,
                    &bm25_field_enum,
                    &values,
                ));
            }
            let full_text = catalog.full_text_indexes_for_table(&table.schema, &table.name);
            if !full_text.is_empty() {
                connection_types_sdl.push_str(&Self::search_field_enum(
                    catalog,
                    &format!("{type_name}FullTextField"),
                    &full_text,
                ));
            }
            let spatial = catalog.spatial_indexes_for_table(&table.schema, &table.name);
            if !spatial.is_empty() {
                connection_types_sdl.push_str(&Self::search_field_enum(
                    catalog,
                    &format!("{type_name}SpatialField"),
                    &spatial,
                ));
            }

            let search_fields = catalog.search_fields(table);
            if !search_fields.is_empty() {
                let mut hit_extra = String::new();
                if full_text.iter().any(|i| {
                    matches!(
                        &i.kind,
                        SearchIndexKind::FullText {
                            source: Some(_),
                            ..
                        }
                    )
                }) {
                    hit_extra.push_str(
                        "  \"\"\"Full-text searches: the fragments of the document's text that match, the query's words marked (null for other searches).\"\"\"\n  \
                         highlight(startSel: String = \"<b>\", stopSel: String = \"</b>\", maxWords: Int, minWords: Int, \
                         \"\"\"Fragments to show (0: the start of the text).\"\"\" maxFragments: Int): String\n",
                    );
                }
                if !spatial.is_empty() {
                    hit_extra.push_str("  \"\"\"Nearest searches: the distance from `near` (null for other searches).\"\"\"\n  distance: Float\n");
                }
                connection_types_sdl.push_str(&format!(
                    "\"\"\"The best matches of a search, best first. Searches are not paginated: a \
                     query that does not surface the expected rows is refined instead.\"\"\"\n\
                     type {type_name}SearchResults {{\n  hits: [{type_name}SearchHit!]!\n  \
                     \"\"\"More rows matched than were returned.\"\"\"\n  hasMore: Boolean!\n}}\n\n\
                     type {type_name}SearchHit {{\n  \
                     \"\"\"Relevance, higher is better; its scale depends on the search method.\"\"\"\n  score: Float!\n  \
                     node: {type_name}!\n{hit_extra}}}\n\n"
                ));
            }

            // 3. Generate Relay Edge and Connection types
            let edge_type_name = format!("{}Edge", type_name);
            let connection_type_name = format!("{}Connection", type_name);

            connection_types_sdl.push_str(&format!("type {} {{\n", edge_type_name));
            connection_types_sdl.push_str("  cursor: String!\n");
            connection_types_sdl.push_str(&format!("  node: {}!\n", type_name));
            if has_aggregates {
                // Window values over every matching row in the connection's order
                let partition = format!("\"\"\"Numbered (or summed) within groups of rows sharing these values.\"\"\" partitionBy: [{type_name}GroupBy!]");
                connection_types_sdl.push_str(&format!(
                    "  \"\"\"Position of the row among all matching rows in the connection's order (1 for the first), counting those before this page.\"\"\"\n  rowNumber({partition}): Int!\n  \
                     \"\"\"Rank of the row in the connection's order (rows with equal sort values share it, leaving gaps).\"\"\"\n  rank({partition}): Int!\n  \
                     denseRank({partition}): Int!\n  \
                     \"\"\"Relative rank, from 0 (first) to 1 (last).\"\"\"\n  percentRank({partition}): Float!\n"
                ));
                if !numeric_cols.is_empty() {
                    connection_types_sdl.push_str(&format!(
                        "  \"\"\"Sums of the matching rows up to this one in the connection's order: running totals (a ledger's balance).\"\"\"\n  runningSum({partition}): {type_name}SumAggregate\n"
                    ));
                }
            }
            connection_types_sdl.push_str("}\n\n");

            connection_types_sdl.push_str(&format!("type {} {{\n", connection_type_name));
            connection_types_sdl.push_str(&format!("  edges: [{}!]!\n", edge_type_name));
            connection_types_sdl.push_str(&format!("  nodes: [{}!]!\n", type_name));
            connection_types_sdl.push_str("  pageInfo: PageInfo!\n");
            connection_types_sdl.push_str(
                "  \"\"\"Number of matching rows (before paging). With `estimate`, the planner's estimate instead, from table statistics, or `EXPLAIN` for filtered rows: cheap on large tables, approximate (connections of a parent row always count exactly). With `exactBelow` too, the rows are counted exactly when the estimate is at most that many (an exact total for a small result, an estimate for a large one).\"\"\"\n  totalCount(estimate: Boolean = false, exactBelow: Int): Int!\n",
            );
            if has_aggregates {
                connection_types_sdl.push_str(&format!(
                    "  aggregates(\"\"\"Only rows matching it are aggregated (the connection's own filter applies too).\"\"\" filter: {type_name}Filter): {type_name}Aggregates\n"
                ));
                connection_types_sdl.push_str(&format!(
                    "  \"\"\"Aggregates of each group of the rows (before paging), ordered by its keys.\"\"\"\n  \
                     groupedAggregates(groupBy: [{type_name}GroupBy!]!, having: {type_name}HavingInput, \
                     \"\"\"Time zone dates and times are truncated in (e.g. `Europe/Paris`); the session's by default.\"\"\" timeZone: String, \
                     \"\"\"Width of `_BINNED` groups, e.g. `PT15M`.\"\"\" binWidth: Interval, \
                     \"\"\"Where `_BINNED` groups start (2000-01-01 by default).\"\"\" binOrigin: Datetime, \
                     \"\"\"Include the empty groups between the first and the last (one truncated or binned key).\"\"\" fillGaps: Boolean = false): [{type_name}AggregateGroup!]!\n"
                ));
            }
            connection_types_sdl.push_str("}\n\n");

            // 4. Generate Table Object Type(s)
            sdl.push_str(&Self::object_type_sdl(catalog, table));

            // 5. Query Root Fields
            let root_doc = |text: String| {
                table.description.as_ref().map(|desc| {
                    Self::format_docstring(Some(&format!("{text}\n{desc}")), "  ")
                        .trim_end_matches('\n')
                        .to_string()
                })
            };
            let ops = table.operations;
            let deprecated = deprecated_directive(table.deprecation.as_deref());
            let first_root = query_fields.len();
            // A. List field: users: [User!]!
            if ops.list {
                query_fields.extend(root_doc(format!("Fetch a list of {} records.", type_name)));
                let args = Self::as_of_arg(table)
                    .map(|a| format!("({a})"))
                    .unwrap_or_default();
                query_fields.push(format!("  {}{args}: [{}!]!", table.list_field, type_name));
            }

            // B. Single row by key (raw key values)
            if ops.read {
                if let Some(args) = Self::key_args(catalog, table) {
                    let doc = if args.contains(" asOf: ") {
                        format!("Lookup a single {type_name} by its key: the version valid at `asOf`, or the latest version.")
                    } else {
                        format!("Lookup a single {} by its primary key.", type_name)
                    };
                    query_fields.extend(root_doc(doc));
                    query_fields.push(format!("  {}({}): {}", table.single_field, args, type_name));
                }
            }
            // B2. Single row by each other unique key: userByEmail(email: String!)
            for (name, cols) in catalog.unique_lookup_fields(table) {
                let args: Vec<String> = cols
                    .iter()
                    .map(|c| format!("{}: {}!", c.field_name, catalog.column_type(c, true)))
                    .collect();
                query_fields.extend(root_doc(format!(
                    "Lookup a single {} by a unique key.",
                    type_name
                )));
                query_fields.push(format!("  {name}({}): {type_name}", args.join(", ")));
            }

            // C. Root Connection fields with orderBy support
            if ops.connection {
                let conn_field_args = format!(
                    "({}): {}!",
                    Self::format_connection_args(table),
                    connection_type_name
                );
                query_fields.extend(root_doc(format!(
                    "Relay connection for {} records with filtering, sorting, and pagination.",
                    type_name
                )));
                query_fields.push(format!("  {}{}", table.connection_field, conn_field_args));
                if let Some(all_conn_field) = &table.all_connection_field {
                    query_fields.push(format!("  {}{}", all_conn_field, conn_field_args));
                }
            }

            // D. Search fields: the best matches, not paginated
            for (name, method) in &search_fields {
                let how = match method {
                    SearchMethod::Fuzzy => "by trigram word similarity",
                    SearchMethod::Semantic => "by vector similarity",
                    SearchMethod::Bm25 => "by BM25 full-text relevance",
                    SearchMethod::Hybrid => "by fused vector and lexical rankings",
                    SearchMethod::FullText => "by full-text relevance",
                    SearchMethod::Nearest => "by distance, nearest",
                };
                query_fields.extend(root_doc(format!(
                    "The {type_name} records best matching a search {how}, best first."
                )));
                let args = Self::search_args(catalog, table, *method);
                query_fields.push(format!("  {name}({args}): {type_name}SearchResults!"));
            }
            // A deprecated type's root fields are deprecated (docstring lines are skipped)
            if !deprecated.is_empty() {
                for line in &mut query_fields[first_root..] {
                    if !line.trim_start().starts_with("\"\"\"")
                        && !line.trim_end().ends_with("\"\"\"")
                    {
                        line.push_str(&deprecated);
                    }
                }
            }
        }

        // Search across every searchable table
        let global_types = catalog.global_search_types();
        if let (Some(name), false) = (
            catalog
                .tables
                .first()
                .and_then(|t| catalog.global_search_field(&t.schema)),
            global_types.is_empty(),
        ) {
            let limits = catalog.search_limits;
            connection_types_sdl.push_str(&format!(
                "\"\"\"A row a global search found.\"\"\"\nunion GlobalSearchNode = {}\n\n\
                 \"\"\"The best matches of a global search, best first (not paginated).\"\"\"\n\
                 type GlobalSearchResults {{\n  hits: [GlobalSearchHit!]!\n  \"\"\"More rows matched than were returned.\"\"\"\n  hasMore: Boolean!\n}}\n\n\
                 type GlobalSearchHit {{\n  \"\"\"Relevance, higher is better: full-text rank, or trigram word similarity for types without a full-text index.\"\"\"\n  score: Float!\n  node: GlobalSearchNode!\n}}\n\n",
                global_types.join(" | ")
            ));
            query_fields.push(
                "  \"\"\"The rows of every searchable type best matching a text, best first: types with a full-text index are searched by it, others by trigram similarity.\"\"\"".to_string(),
            );
            query_fields.push(format!(
                "  {name}(\"\"\"Text to search for (web search syntax for full-text searches).\"\"\" query: String!, \"\"\"Number of hits: {} by default, at most {}.\"\"\" first: Int): GlobalSearchResults!",
                limits.default, limits.max
            ));
        }

        // Root query fields of functions that do not change data
        for func in catalog.function_root_fields(false) {
            if let Some(desc) = &func.description {
                let doc = Self::format_docstring(Some(desc), "  ");
                query_fields.push(doc.trim_end_matches('\n').to_string());
            }
            query_fields.push(format!(
                "  {}{}",
                Self::function_field_sdl(catalog, func, false),
                deprecated_directive(func.deprecation.as_deref())
            ));
        }

        for (schema, name) in &to_many_filters {
            if let Some(target) = catalog.find_table(schema, name) {
                connection_types_sdl.push_str(&Self::to_many_filter_sdl(target));
            }
        }
        sdl.push_str(&connection_types_sdl);

        // Mutation inputs, payloads and root fields
        let mut mutation_fields = Vec::new();
        let mut mutation_types_sdl = String::new();
        let mut taken_mutations: HashSet<String> = HashSet::new();

        if catalog
            .tables
            .iter()
            .any(|t| t.has_upsert() && (t.operations.create || t.operations.create_many))
        {
            mutation_types_sdl.push_str(concat!(
                "\"\"\"What an insert does when a row conflicts with an existing one on a unique key.\"\"\"\n",
                "enum ConflictAction {\n",
                "  \"\"\"Update the existing row from the proposed one.\"\"\"\n  UPDATE\n",
                "  \"\"\"Leave the existing row as it is.\"\"\"\n  NOTHING\n",
                "}\n\n",
            ));
        }

        // Relation inputs of nested writes, by target table: [to-one, to-many] inputs used by
        // creates, then by updates
        let mut nested_targets: Vec<(&TableMetadata, [bool; 4])> = Vec::new();

        for table in &catalog.tables {
            let type_name = &table.type_name;
            let writable: Vec<&ColumnMetadata> = table.writable_columns().collect();
            mutation_types_sdl.push_str(&Self::error_code_sdl(catalog, table));

            // Create input, also used by nested creates (an input type needs at least one
            // field: without writable columns only a single-row create exists, taking no input)
            if table.is_insertable() {
                let nested = catalog.nested_relations(table);
                // A foreign key column a relation field can fill is optional
                let filled: HashSet<&str> = nested
                    .iter()
                    .filter(|(_, rel)| !rel.is_many)
                    .flat_map(|(_, rel)| rel.columns.iter().map(|(_, fk)| fk.as_str()))
                    .collect();
                mutation_types_sdl.push_str(&format!("input Create{type_name}Input {{\n"));
                for col in &writable {
                    let required = !col.is_nullable
                        && col.default_value.is_none()
                        && !col.is_primary_key
                        && !filled.contains(col.name.as_str());
                    let non_null = if required { "!" } else { "" };
                    mutation_types_sdl.push_str(&Self::format_docstring(
                        catalog.column_description(table, col).as_deref(),
                        "  ",
                    ));
                    mutation_types_sdl.push_str(&format!(
                        "  {}: {}{}{}\n",
                        col.field_name,
                        catalog.column_type(col, true),
                        non_null,
                        Self::constraint_directive(table, col)
                    ));
                }
                for (name, rel) in &nested {
                    let Some(target) = catalog.find_table(&rel.target_schema, &rel.target_table)
                    else {
                        continue;
                    };
                    let (suffix, doc) = match (rel.is_many, rel.junction.is_some()) {
                        (false, _) => (
                            "ToOne",
                            format!(
                                "The `{}` this row refers to: an existing one, or a new one.",
                                target.type_name
                            ),
                        ),
                        (true, false) => (
                            "ToMany",
                            format!(
                                "`{}` rows to create with, or move to, this row.",
                                target.type_name
                            ),
                        ),
                        (true, true) => (
                            "ToMany",
                            format!(
                                "`{}` rows to link this row to: existing ones, or new ones.",
                                target.type_name
                            ),
                        ),
                    };
                    mutation_types_sdl.push_str(&format!(
                        "  \"\"\"{doc}\"\"\"\n  {name}: {}{suffix}Input\n",
                        target.type_name
                    ));
                    Self::mark_relation_input(
                        &mut nested_targets,
                        target,
                        usize::from(rel.is_many),
                    );
                }
                mutation_types_sdl.push_str("}\n\n");
            }

            // The first table claiming a field name wins, as in the catalog index
            let fields: Vec<MutationField> = catalog
                .table_mutation_fields(table)
                .into_iter()
                .filter(|f| taken_mutations.insert(f.name.clone()))
                .collect();
            if fields.is_empty() {
                continue;
            }
            let has = |pred: &dyn Fn(MutationAction) -> bool| fields.iter().any(|f| pred(f.action));
            let creates = has(&|a| a.is_create()) && !writable.is_empty();
            let updates =
                has(&|a| matches!(a, MutationAction::Update | MutationAction::UpdateMany));
            let upsert = creates && table.has_upsert();
            if updates {
                mutation_types_sdl.push_str(&format!("input Update{type_name}Input {{\n"));
                for col in &writable {
                    mutation_types_sdl.push_str(&Self::format_docstring(
                        catalog.column_description(table, col).as_deref(),
                        "  ",
                    ));
                    mutation_types_sdl.push_str(&format!(
                        "  {}: {}{}\n",
                        col.field_name,
                        catalog.column_type(col, true),
                        Self::constraint_directive(table, col)
                    ));
                }
                // Nested writes of single-row updates, which read the row back by key
                if has(&|a| a == MutationAction::Update) && !table.primary_key.is_empty() {
                    for (name, rel) in &catalog.nested_relations(table) {
                        let Some(target) =
                            catalog.find_table(&rel.target_schema, &rel.target_table)
                        else {
                            continue;
                        };
                        let (suffix, doc) = if rel.is_many {
                            ("ToManyUpdate", format!("`{}` rows to relate to, or detach from, this row (single-row updates only).", target.type_name))
                        } else {
                            ("ToOneUpdate", format!("The `{}` this row refers to: another existing one, a new one, or none (single-row updates only).", target.type_name))
                        };
                        mutation_types_sdl.push_str(&format!(
                            "  \"\"\"{doc}\"\"\"\n  {name}: {}{suffix}Input\n",
                            target.type_name
                        ));
                        Self::mark_relation_input(
                            &mut nested_targets,
                            target,
                            2 + usize::from(rel.is_many),
                        );
                    }
                }
                mutation_types_sdl.push_str("}\n\n");
            }
            if upsert {
                mutation_types_sdl.push_str(&Self::on_conflict_sdl(table, &writable));
            }

            let mut payloads: HashSet<String> = HashSet::new();
            for field in &fields {
                let action = field.action;
                let payload_name = action.payload_type_name(table);
                if payloads.insert(payload_name.clone()) {
                    let rows_field = action.payload_field_name(table);
                    let mut body = if action.is_many() {
                        format!(
                            "  \"\"\"The affected rows.\"\"\"\n  {rows_field}: [{type_name}!]!\n  \"\"\"Number of rows inserted, updated or deleted.\"\"\"\n  affectedCount: Int!\n"
                        )
                    } else {
                        let non_null = if action == MutationAction::Create {
                            "!"
                        } else {
                            ""
                        };
                        format!("  {rows_field}: {type_name}{non_null}\n")
                    };
                    if let Some(previous) = catalog.previous_field_name(table, action, upsert) {
                        let (doc, ty) = match (action, action.is_many()) {
                            (MutationAction::Update | MutationAction::UpdateMany, false) => ("The row as it was before the update.", type_name.clone()),
                            (MutationAction::Update | MutationAction::UpdateMany, true) => ("The rows as they were before the update, in the order of `{rows}`.", format!("[{type_name}!]!")),
                            (_, false) => ("The row as it was before an upsert updated it; null when it was inserted.", type_name.clone()),
                            (_, true) => ("The rows as they were before an upsert updated them, in the order of `{rows}`; null for rows it inserted.", format!("[{type_name}]!")),
                        };
                        let doc = doc.replace("{rows}", &rows_field);
                        body.push_str(&format!("  \"\"\"{doc} Its relations are read as they are now.\"\"\"\n  {previous}: {ty}\n"));
                    }
                    mutation_types_sdl.push_str(&format!("type {payload_name} {{\n{body}}}\n\n"));
                }

                let unique_key: Vec<&ColumnMetadata> = field
                    .unique_key
                    .iter()
                    .filter_map(|c| table.get_column(c))
                    .collect();
                // The row's key, typed as queries take it (every column of a temporal key)
                let key_columns: Vec<&ColumnMetadata> = if unique_key.is_empty() {
                    table.key_columns().unwrap_or_default()
                } else {
                    unique_key.clone()
                };
                let key_args = if field.by_node_id {
                    "nodeId: ID!".to_string()
                } else {
                    key_columns
                        .iter()
                        .map(|c| format!("{}: {}!", c.field_name, catalog.column_type(c, true)))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let key_names = unique_key
                    .iter()
                    .map(|c| format!("`{}`", c.field_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let expected_version = if table.has_row_version() {
                    ", \"\"\"Write only if the row's `rowVersion` is still this one (else `STALE_VERSION`).\"\"\" expectedVersion: String"
                } else {
                    ""
                };
                let on_conflict = if upsert {
                    format!(", onConflict: {type_name}OnConflict")
                } else {
                    String::new()
                };
                let (doc, args) = match action {
                    MutationAction::Create if writable.is_empty() => (String::new(), String::new()),
                    MutationAction::Create => (String::new(), format!("input: Create{type_name}Input!{on_conflict}")),
                    MutationAction::CreateMany => (
                        format!("Insert several `{type_name}` rows; the payload lists them in input order."),
                        format!("inputs: [Create{type_name}Input!]!{on_conflict}"),
                    ),
                    MutationAction::Update => (
                        if field.by_node_id { format!("Update the `{type_name}` whose global `id` is `nodeId`.") } else if unique_key.is_empty() { String::new() } else { format!("Update the `{type_name}` with the given {key_names}.") },
                        format!("{}, input: Update{type_name}Input!{expected_version}", key_args),
                    ),
                    MutationAction::UpdateMany => (
                        format!("Update every `{type_name}` matching `where`."),
                        format!("where: {type_name}Filter!, set: Update{type_name}Input!, \"\"\"Allow a `where` without conditions, which updates every row (see `limits.require_filter`).\"\"\" unfiltered: Boolean = false"),
                    ),
                    MutationAction::Delete => (
                        if field.by_node_id { format!("Delete the `{type_name}` whose global `id` is `nodeId`.") } else if unique_key.is_empty() { String::new() } else { format!("Delete the `{type_name}` with the given {key_names}.") },
                        format!("{}{expected_version}", key_args),
                    ),
                    MutationAction::DeleteMany => {
                        (format!("Delete every `{type_name}` matching `where`."), format!("where: {type_name}Filter!, \"\"\"Allow a `where` without conditions, which deletes every row (see `limits.require_filter`).\"\"\" unfiltered: Boolean = false"))
                    }
                };
                mutation_fields.push(Self::format_docstring(
                    (!doc.is_empty()).then_some(doc.as_str()),
                    "  ",
                ));
                let args = if args.is_empty() {
                    String::new()
                } else {
                    format!("({args})")
                };
                let deprecated = deprecated_directive(table.deprecation.as_deref());
                mutation_fields.push(format!(
                    "  {}{args}: {payload_name}!{deprecated}\n",
                    field.name
                ));
            }
        }

        // Mutation fields of `VOLATILE` functions
        for func in catalog.function_root_fields(true) {
            mutation_fields.push(Self::format_docstring(func.description.as_deref(), "  "));
            mutation_fields.push(format!(
                "  {}{}\n",
                Self::function_field_sdl(catalog, func, true),
                deprecated_directive(func.deprecation.as_deref())
            ));
        }

        for (target, used) in nested_targets {
            mutation_types_sdl.push_str(&Self::relation_inputs_sdl(catalog, target, used));
        }

        sdl.push_str(&mutation_types_sdl);

        // Generate Query root type with Relay node(id: ID!) entry point
        sdl.push_str("type Query {\n");
        if !for_subgraph_service && catalog.federation_enabled {
            sdl.push_str("  _service: _Service!\n");
            if !entity_type_names.is_empty() {
                sdl.push_str("  _entities(representations: [_Any!]!): [_Entity]!\n");
            }
        }
        // Relay Node interface entry point
        sdl.push_str("  node(id: ID!): Node\n");
        for qf in &query_fields {
            sdl.push_str(&format!("{}\n", qf));
        }
        sdl.push_str("}\n\n");

        // Live queries: the Query type's fields, each result sent again when it changes
        if catalog.subscriptions_enabled {
            sdl.push_str("\"\"\"Live queries: the result of a `Query` field, sent again whenever it changes.\"\"\"\ntype Subscription {\n");
            sdl.push_str("  node(id: ID!): Node\n");
            for qf in &query_fields {
                sdl.push_str(&format!("{}\n", qf));
            }
            sdl.push_str("}\n\n");
        }

        // Generate Mutation root type
        if !mutation_fields.is_empty() {
            sdl.push_str("type Mutation {\n");
            sdl.push_str(&mutation_fields.concat());
            sdl.push_str("}\n");
        }

        sdl
    }

    /// Inputs of relations to `table` in nested creates: `<Type>KeyInput` (identifies an
    /// existing row), `<Type>ToOneInput` and `<Type>ToManyInput`.
    /// Records that relation input `slot` (see `relation_inputs_sdl`) of `target` is used.
    fn mark_relation_input<'c>(
        targets: &mut Vec<(&'c TableMetadata, [bool; 4])>,
        target: &'c TableMetadata,
        slot: usize,
    ) {
        match targets.iter_mut().find(|(t, _)| std::ptr::eq(*t, target)) {
            Some((_, used)) => used[slot] = true,
            None => {
                let mut used = [false; 4];
                used[slot] = true;
                targets.push((target, used));
            }
        }
    }

    /// Relation inputs of `table` that `used` flags: `[<Type>ToOneInput, <Type>ToManyInput,
    /// <Type>ToOneUpdateInput, <Type>ToManyUpdateInput]` (and `<Type>KeyInput`).
    fn relation_inputs_sdl(
        catalog: &DatabaseCatalog,
        table: &TableMetadata,
        used: [bool; 4],
    ) -> String {
        let [to_one, to_many, to_one_update, to_many_update] = used;
        let type_name = &table.type_name;
        let mut sdl = String::new();
        let connect = table.is_connectable();
        let create = table.is_insertable();
        let updatable = table.operations.update
            && !table.primary_key.is_empty()
            && table.writable_columns().next().is_some();
        let connect_or_create = connect && create;
        if connect_or_create {
            sdl.push_str(&format!(
                "\"\"\"The `{type_name}` with a key, created if there is none (its nested relations are not written).\"\"\"\ninput {type_name}ConnectOrCreateInput {{\n  \
                 where: {type_name}KeyInput!\n  create: Create{type_name}Input!\n}}\n\n"
            ));
        }
        if to_many_update && connect && updatable {
            sdl.push_str(&format!(
                "\"\"\"Changes to a related `{type_name}`, found by key (its nested relations are not written).\"\"\"\ninput {type_name}UpdateByKeyInput {{\n  \
                 where: {type_name}KeyInput!\n  data: Update{type_name}Input!\n}}\n\n"
            ));
        }
        if connect {
            let keys: Vec<String> = table
                .row_keys()
                .iter()
                .map(|fields| {
                    fields
                        .iter()
                        .map(|(f, _)| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                })
                .collect();
            sdl.push_str(&format!(
                "\"\"\"Identifies a `{type_name}` by {}.\"\"\"\ninput {type_name}KeyInput {{\n",
                keys.join(", or by ")
            ));
            let mut seen = HashSet::new();
            for (field, col) in table.row_keys().into_iter().flatten() {
                if seen.insert(field.clone()) {
                    let ty = if field == "id" && table.implements_node() {
                        "ID".to_string()
                    } else {
                        catalog.column_type(col, true)
                    };
                    sdl.push_str(&format!("  {field}: {ty}\n"));
                }
            }
            sdl.push_str("}\n\n");
        }
        if to_one {
            sdl.push_str(&format!("\"\"\"A `{type_name}` to refer to: set one of `connect` and `create`.\"\"\"\ninput {type_name}ToOneInput {{\n"));
            if connect {
                sdl.push_str(&format!(
                    "  \"\"\"An existing `{type_name}`.\"\"\"\n  connect: {type_name}KeyInput\n"
                ));
            }
            if create {
                sdl.push_str(&format!("  \"\"\"A new `{type_name}`, created first.\"\"\"\n  create: Create{type_name}Input\n"));
            }
            if connect_or_create {
                sdl.push_str(&format!("  \"\"\"An existing `{type_name}`, or a new one if none has the key.\"\"\"\n  connectOrCreate: {type_name}ConnectOrCreateInput\n"));
            }
            sdl.push_str("}\n\n");
        }
        if to_many {
            sdl.push_str(&format!("\"\"\"`{type_name}` rows to relate to the new row.\"\"\"\ninput {type_name}ToManyInput {{\n"));
            if create {
                sdl.push_str(&format!("  \"\"\"New `{type_name}` rows, created after it.\"\"\"\n  create: [Create{type_name}Input!]\n"));
            }
            if connect {
                sdl.push_str(&format!("  \"\"\"Existing `{type_name}` rows.\"\"\"\n  connect: [{type_name}KeyInput!]\n"));
            }
            if connect_or_create {
                sdl.push_str(&format!("  \"\"\"Existing `{type_name}` rows, or new ones for keys no row has.\"\"\"\n  connectOrCreate: [{type_name}ConnectOrCreateInput!]\n"));
            }
            sdl.push_str("}\n\n");
        }
        if to_one_update {
            sdl.push_str(&format!(
                "\"\"\"The `{type_name}` an updated row refers to: set one of `connect`, `create` and `disconnect`.\"\"\"\ninput {type_name}ToOneUpdateInput {{\n"
            ));
            if connect {
                sdl.push_str(&format!(
                    "  \"\"\"An existing `{type_name}`.\"\"\"\n  connect: {type_name}KeyInput\n"
                ));
            }
            if create {
                sdl.push_str(&format!("  \"\"\"A new `{type_name}`, created first.\"\"\"\n  create: Create{type_name}Input\n"));
            }
            if connect_or_create {
                sdl.push_str(&format!("  \"\"\"An existing `{type_name}`, or a new one if none has the key.\"\"\"\n  connectOrCreate: {type_name}ConnectOrCreateInput\n"));
            }
            if updatable {
                sdl.push_str(&format!("  \"\"\"Changes to the `{type_name}` it refers to (which stays the same).\"\"\"\n  update: Update{type_name}Input\n"));
            }
            sdl.push_str("  \"\"\"Refer to none (the foreign key becomes NULL).\"\"\"\n  disconnect: Boolean\n}\n\n");
        }
        if to_many_update {
            sdl.push_str(&format!("\"\"\"`{type_name}` rows to relate to, or detach from, the updated row.\"\"\"\ninput {type_name}ToManyUpdateInput {{\n"));
            if create {
                sdl.push_str(&format!("  \"\"\"New `{type_name}` rows, related to it.\"\"\"\n  create: [Create{type_name}Input!]\n"));
            }
            if connect {
                sdl.push_str(&format!(
                    "  \"\"\"Existing `{type_name}` rows to relate to it.\"\"\"\n  connect: [{type_name}KeyInput!]\n  \
                     \"\"\"Related `{type_name}` rows to detach (a child's foreign key becomes NULL; a link is removed).\"\"\"\n  disconnect: [{type_name}KeyInput!]\n  \
                     \"\"\"The related `{type_name}` rows, exactly: others are detached, these related.\"\"\"\n  set: [{type_name}KeyInput!]\n"
                ));
            }
            if connect_or_create {
                sdl.push_str(&format!("  \"\"\"Existing `{type_name}` rows to relate to it, or new ones for keys no row has.\"\"\"\n  connectOrCreate: [{type_name}ConnectOrCreateInput!]\n"));
            }
            if connect && updatable {
                sdl.push_str(&format!(
                    "  \"\"\"Changes to related `{type_name}` rows (its own children only, not linked rows).\"\"\"\n  update: [{type_name}UpdateByKeyInput!]\n"
                ));
            }
            if connect && table.operations.delete {
                sdl.push_str(&format!("  \"\"\"Related `{type_name}` rows to delete (its own children only).\"\"\"\n  delete: [{type_name}KeyInput!]\n"));
            }
            let sync =
                create && updatable && table.operations.delete && catalog.server_version >= 170000;
            if sync {
                sdl.push_str(&format!(
                    "  \"\"\"The related `{type_name}` rows, exactly (its own children only, one `MERGE`): rows whose key matches a child update it, \
                     the others are created, and children none matches are deleted. Every row sets the same fields.\"\"\"\n  sync: [Create{type_name}Input!]\n"
                ));
            }
            sdl.push_str("}\n\n");
        }
        sdl
    }

    /// `enum <Type>ErrorCode`: why a write of `table`'s rows can be rejected, derived from its
    /// constraints and column types. Tables that cannot be written get none.
    fn error_code_sdl(catalog: &DatabaseCatalog, table: &TableMetadata) -> String {
        let ops = table.operations;
        let writable = ops.create
            || ops.update
            || ops.delete
            || ops.create_many
            || ops.update_many
            || ops.delete_many;
        let codes = catalog.error_codes(table);
        if !writable || codes.is_empty() {
            return String::new();
        }
        let type_name = &table.type_name;
        let mut sdl = format!(
            "\"\"\"Why a write of `{type_name}` rows was rejected: the `extensions.code` of the error, whose \
             `extensions.fields` name the input fields holding the offending values.\"\"\"\nenum {type_name}ErrorCode {{\n"
        );
        for (code, doc) in codes {
            sdl.push_str(&Self::format_docstring(Some(&doc), "  "));
            sdl.push_str(&format!("  {code}\n"));
        }
        sdl.push_str("}\n\n");
        sdl
    }

    /// `<Type>ConflictKey`, `<Type>UpdateColumn` and `<Type>OnConflict`: the `onConflict`
    /// argument of `table`'s creates.
    fn on_conflict_sdl(table: &TableMetadata, writable: &[&ColumnMetadata]) -> String {
        let type_name = &table.type_name;
        let mut sdl = format!("\"\"\"Unique keys of `{type_name}` an insert can conflict on.\"\"\"\nenum {type_name}ConflictKey {{\n");
        let mut has_primary = false;
        for (key, cols) in table.conflict_keys() {
            has_primary |= key.is_primary;
            let fields = cols
                .iter()
                .map(|c| format!("`{}`", c.field_name))
                .collect::<Vec<_>>()
                .join(", ");
            sdl.push_str(&format!(
                "  \"\"\"{fields}\"\"\"\n  {}\n",
                conflict_key_value(key, &cols)
            ));
        }
        sdl.push_str("}\n\n");

        sdl.push_str(&format!("\"\"\"Fields of `{type_name}` an insert can update on conflict.\"\"\"\nenum {type_name}UpdateColumn {{\n"));
        let mut seen = HashSet::new();
        for col in writable {
            if seen.insert(col.order_by_name()) {
                sdl.push_str(&format!("  {}\n", col.order_by_name()));
            }
        }
        sdl.push_str("}\n\n");

        let key_default = if has_primary { " = PRIMARY_KEY" } else { "" };
        sdl.push_str(&format!(
            concat!(
                "\"\"\"What an insert does when a row has the same unique key as an existing `{t}`.\"\"\"\n",
                "input {t}OnConflict {{\n",
                "  \"\"\"The unique key rows are matched by; every inserted row must set its fields.\"\"\"\n",
                "  key: {t}ConflictKey!{d}\n",
                "  action: ConflictAction! = UPDATE\n",
                "  \"\"\"Fields updated from the proposed row. Default: the fields it sets, apart from the key and the primary key.\"\"\"\n",
                "  update: [{t}UpdateColumn!]\n",
                "}}\n\n",
            ),
            t = type_name,
            d = key_default
        ));
        sdl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_docstring_ends_with_quote() {
        let doc = SchemaBuilder::format_docstring(
            Some("Warning such as \"Pregnancy\", \"Overdose\""),
            "  ",
        );
        assert_eq!(
            doc,
            "  \"\"\"Warning such as \"Pregnancy\", \"Overdose\" \"\"\"\n"
        );

        let sdl = format!(
            "{}type TestQuery {{\n  field: String\n}}\nschema {{\n  query: TestQuery\n}}",
            doc
        );
        let schema_res = apollo_compiler::Schema::parse_and_validate(&sdl, "test.graphql");
        assert!(
            schema_res.is_ok(),
            "Validation failed: {:?}",
            schema_res.err()
        );
    }
}
