//! Applies an [`EngineConfig`] to an inspected catalog, in two phases:
//!
//! - [`apply_structure`] runs before relations, polymorphism and many-to-many links are
//!   detected: it adds the primary keys and foreign keys of relations that cannot declare
//!   them (views) and the function mounts, so detection sees them like database constraints.
//! - [`apply_presentation`] runs once default GraphQL names are assigned: renames,
//!   descriptions, omitted fields, operations and types.
//!
//! Both validate every reference and return all problems at once.
use super::models::{EngineConfig, Operation};
use crate::catalog::{is_graphql_name, DatabaseCatalog, ForeignKeyMetadata, SearchLimits, TableMetadata};

type Errors = Vec<String>;

fn check_name(errors: &mut Errors, path: &str, name: &str) -> bool {
    let ok = is_graphql_name(name);
    if !ok {
        errors.push(format!("{path}: `{name}` is not a valid GraphQL name"));
    }
    ok
}

/// Position, among `objects` (`(schema, name)` pairs), of the object a config key names:
/// `schema.name`, or a bare name. A bare name several schemas share resolves to the default
/// schema's (the first of the config's `schemas`); without one there it is ambiguous. `Err`
/// says why nothing was resolved, `kind` naming the objects searched.
fn resolve_name<'a>(
    objects: impl Iterator<Item = (&'a str, &'a str)>,
    config: &EngineConfig,
    name: &str,
    kind: &str,
) -> Result<usize, String> {
    let matches: Vec<(usize, &str)> = match name.split_once('.') {
        Some((schema, object)) => objects.enumerate().filter(|(_, (s, n))| *s == schema && *n == object).map(|(i, (s, _))| (i, s)).collect(),
        None => objects.enumerate().filter(|(_, (_, n))| *n == name).map(|(i, (s, _))| (i, s)).collect(),
    };
    match matches.as_slice() {
        [] => Err(format!("no {kind} `{name}` in the inspected schemas")),
        [(i, _)] => Ok(*i),
        many => match many.iter().find(|(_, s)| config.schemas.first().is_some_and(|d| d == s)) {
            Some((i, _)) => Ok(*i),
            None => {
                let schemas: Vec<&str> = many.iter().map(|(_, s)| *s).collect();
                Err(format!("`{name}` names a {kind} in several schemas ({}), qualify it as `<schema>.{name}`", schemas.join(", ")))
            }
        },
    }
}

const RELATION: &str = "table, view, materialized view or foreign table";

fn table_index(catalog: &DatabaseCatalog, config: &EngineConfig, name: &str, kind: &str) -> Result<usize, String> {
    resolve_name(catalog.tables.iter().map(|t| (t.schema.as_str(), t.name.as_str())), config, name, kind)
}

/// The relation an `entities` key names.
fn find_table<'c>(catalog: &'c DatabaseCatalog, config: &EngineConfig, name: &str) -> Result<&'c TableMetadata, String> {
    table_index(catalog, config, name, RELATION).map(|i| &catalog.tables[i])
}

fn find_table_mut<'c>(catalog: &'c mut DatabaseCatalog, config: &EngineConfig, name: &str) -> Result<&'c mut TableMetadata, String> {
    table_index(catalog, config, name, RELATION).map(|i| &mut catalog.tables[i])
}

/// The relation a reference (a foreign key target, a mount parent) names.
fn find_relation<'c>(catalog: &'c DatabaseCatalog, config: &EngineConfig, name: &str) -> Result<&'c TableMetadata, String> {
    table_index(catalog, config, name, "relation").map(|i| &catalog.tables[i])
}

fn function_index(catalog: &DatabaseCatalog, config: &EngineConfig, name: &str) -> Result<usize, String> {
    resolve_name(catalog.functions.iter().map(|f| (f.schema.as_str(), f.name.as_str())), config, name, "function")
}

/// Declared keys, virtual foreign keys and function mounts.
pub fn apply_structure(catalog: &mut DatabaseCatalog, config: &EngineConfig) -> Result<(), Errors> {
    let mut errors = Errors::new();

    // Primary keys first: a virtual foreign key may reference a declared key
    for (name, entity) in &config.entities {
        let path = format!("entities.{name}");
        let table = match find_table_mut(catalog, config, name) {
            Ok(table) => table,
            Err(err) => {
                errors.push(format!("{path}: {err}"));
                continue;
            }
        };
        if let Some(column) = &entity.version_column {
            if table.get_column(column).is_some() {
                table.version_column = Some(column.clone());
            } else {
                errors.push(format!("{path}.version_column: no column `{column}`"));
            }
        }
        if entity.primary_key.is_empty() {
            continue;
        }
        if !table.primary_key.is_empty() && table.primary_key != entity.primary_key {
            errors.push(format!(
                "{path}.primary_key: `{name}` already has primary key ({})",
                table.primary_key.join(", ")
            ));
            continue;
        }
        let missing: Vec<&String> = entity.primary_key.iter().filter(|c| table.get_column(c).is_none()).collect();
        if !missing.is_empty() {
            for c in missing {
                errors.push(format!("{path}.primary_key: no column `{c}`"));
            }
            continue;
        }
        for c in &mut table.columns {
            c.is_primary_key = entity.primary_key.contains(&c.name);
        }
        table.primary_key = entity.primary_key.clone();
    }

    for (name, entity) in &config.entities {
        for (fk_name, fk) in &entity.foreign_keys {
            let path = format!("entities.{name}.foreign_keys.{fk_name}");
            let Ok(table) = find_table(catalog, config, name) else { continue };
            let declares = !fk.columns.is_empty() || fk.references.is_some();
            if table.foreign_keys.iter().any(|f| &f.constraint_name == fk_name) {
                if declares {
                    errors.push(format!(
                        "{path}: `{fk_name}` is an existing constraint; `columns` and `references` only declare new foreign keys"
                    ));
                }
                continue;
            }
            let Some(reference) = fk.references.as_ref().filter(|_| !fk.columns.is_empty()) else {
                errors.push(format!(
                    "{path}: `{name}` has no foreign key `{fk_name}`; to declare one, set `columns` and `references`"
                ));
                continue;
            };
            let mut ok = true;
            for c in &fk.columns {
                if table.get_column(c).is_none() {
                    errors.push(format!("{path}.columns: no column `{c}` in `{name}`"));
                    ok = false;
                }
            }
            let target = match find_relation(catalog, config, &reference.table) {
                Ok(target) => target,
                Err(err) => {
                    errors.push(format!("{path}.references.table: {err}"));
                    continue;
                }
            };
            let foreign_columns =
                if reference.columns.is_empty() { target.primary_key.clone() } else { reference.columns.clone() };
            if foreign_columns.is_empty() {
                errors.push(format!(
                    "{path}.references.columns: `{}` has no primary key, name the referenced columns",
                    reference.table
                ));
                continue;
            }
            for c in &foreign_columns {
                if target.get_column(c).is_none() {
                    errors.push(format!("{path}.references.columns: no column `{c}` in `{}`", reference.table));
                    ok = false;
                }
            }
            if foreign_columns.len() != fk.columns.len() {
                errors.push(format!(
                    "{path}: {} referencing column(s) for {} referenced column(s)",
                    fk.columns.len(),
                    foreign_columns.len()
                ));
                ok = false;
            }
            if !ok {
                continue;
            }
            let virtual_fk = ForeignKeyMetadata {
                constraint_name: fk_name.clone(),
                columns: fk.columns.clone(),
                foreign_schema: target.schema.clone(),
                foreign_table: target.name.clone(),
                foreign_columns,
                is_virtual: true,
                field_name: None,
                reverse_field_name: None,
                omit: false,
                omit_reverse: false,
                period: false,
            };
            if let Ok(table) = find_table_mut(catalog, config, name) {
                table.foreign_keys.push(virtual_fk);
            }
        }
    }

    for (name, function) in &config.functions {
        let path = format!("functions.{name}");
        let Some(mount) = &function.mount else { continue };
        let idx = match function_index(catalog, config, name) {
            Ok(idx) => idx,
            Err(err) => {
                errors.push(format!("{path}: {err}"));
                continue;
            }
        };
        let func = &catalog.functions[idx];
        if func.row_argument {
            errors.push(format!("{path}.mount: `{name}` takes a row of `{}`, so it is already a field of its type", func.parent_table.as_deref().unwrap_or("?")));
            continue;
        }
        if !mount.omit && !(func.returns_record() && func.returns_set && !func.volatile) {
            errors.push(format!(
                "{path}.mount: only non-volatile functions returning `TABLE (...)` columns (or `SETOF` OUT parameters) can be mounted"
            ));
            continue;
        }
        if mount.omit {
            let func = &mut catalog.functions[idx];
            func.parent_table = None;
            func.parent_schema = None;
            func.parent_arguments.clear();
            continue;
        }
        let func = &catalog.functions[idx];
        let Some(on) = mount.on.clone().or_else(|| func.parent_table.clone()) else {
            errors.push(format!("{path}.mount.on: no parent detected for `{name}`, name the relation to mount it on"));
            continue;
        };
        // A detected parent is in the function's schema; a configured one is resolved by name
        let parent = match (&mount.on, &func.parent_schema) {
            (None, None) => find_relation(catalog, config, &format!("{}.{on}", func.schema)),
            (None, Some(schema)) => find_relation(catalog, config, &format!("{schema}.{on}")),
            (Some(_), _) => find_relation(catalog, config, &on),
        };
        let parent = match parent {
            Ok(parent) => parent,
            Err(err) => {
                errors.push(format!("{path}.mount.on: {err}"));
                continue;
            }
        };
        let parent_arguments: Vec<(String, String)> = if !mount.arguments.is_empty() {
            let mut ok = true;
            for (arg, column) in &mount.arguments {
                if !func.arguments.iter().any(|a| &a.name == arg) {
                    errors.push(format!("{path}.mount.arguments: `{name}` has no argument `{arg}`"));
                    ok = false;
                }
                if parent.get_column(column).is_none() {
                    errors.push(format!("{path}.mount.arguments.{arg}: no column `{column}` in `{on}`"));
                    ok = false;
                }
            }
            if !ok {
                continue;
            }
            // In argument order
            func.arguments
                .iter()
                .filter_map(|a| mount.arguments.get(&a.name).map(|c| (a.name.clone(), c.clone())))
                .collect()
        } else if func.parent_table.as_deref() == Some(parent.name.as_str())
            && func.parent_schema.as_deref().unwrap_or(&func.schema) == parent.schema
        {
            func.parent_arguments.clone()
        } else {
            match (func.arguments.first(), parent.primary_key.as_slice()) {
                (Some(arg), [pk]) => vec![(arg.name.clone(), pk.clone())],
                (None, _) => {
                    errors.push(format!("{path}.mount: `{name}` takes no arguments to pass the parent row in"));
                    continue;
                }
                _ => {
                    errors.push(format!(
                        "{path}.mount.arguments: `{on}` has no single-column primary key to pass as the first argument, map arguments to columns"
                    ));
                    continue;
                }
            }
        };
        let (parent_schema, parent_name) = (parent.schema.clone(), parent.name.clone());
        let func = &mut catalog.functions[idx];
        func.parent_schema = (parent_schema != func.schema).then_some(parent_schema);
        func.parent_table = Some(parent_name);
        func.parent_arguments = parent_arguments;
        func.mounted_field_name.clear();
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Names, descriptions, omitted fields, operations and types.
pub fn apply_presentation(catalog: &mut DatabaseCatalog, config: &EngineConfig) -> Result<(), Errors> {
    let mut errors = Errors::new();
    let mut omitted = Vec::new();

    let defaults = SearchLimits::default();
    let max = config.search.max_limit.unwrap_or(defaults.max);
    let default = config.search.default_limit.unwrap_or(defaults.default.min(max));
    if max == 0 {
        errors.push("search.max_limit: must be at least 1".to_string());
    } else if default == 0 || default > max {
        errors.push(format!("search.default_limit: must be between 1 and search.max_limit ({max})"));
    } else {
        catalog.search_limits = SearchLimits { default, max };
    }

    catalog.federation_enabled = config.federation.enabled;
    catalog.federation_version = config.federation.version.clone();

    for (name, entity) in &config.entities {
        let path = format!("entities.{name}");
        let table = match find_table_mut(catalog, config, name) {
            Ok(table) => table,
            Err(err) => {
                errors.push(format!("{path}: {err}"));
                continue;
            }
        };
        if entity.omit {
            omitted.push((table.schema.clone(), table.name.clone()));
        }
        if let Some(type_name) = &entity.type_name {
            if check_name(&mut errors, &format!("{path}.type_name"), type_name) {
                table.type_name = type_name.clone();
            }
        }
        if let Some(description) = &entity.description {
            table.description = Some(description.clone());
        }
        if let Some(reason) = &entity.deprecated {
            table.deprecation = Some(reason.clone());
        }
        if let Some(key) = &entity.key_fields {
            table.federation_key = Some(key.clone());
        }
        if let Some(shareable) = entity.shareable {
            table.federation_shareable = shareable;
        }
        for (key, value) in [
            ("list_field", &entity.list_field),
            ("single_field", &entity.single_field),
            ("connection_field", &entity.connection_field),
        ] {
            let Some(value) = value else { continue };
            if !check_name(&mut errors, &format!("{path}.{key}"), value) {
                continue;
            }
            match key {
                "list_field" => table.list_field = value.clone(),
                "single_field" => table.single_field = value.clone(),
                _ => {
                    table.connection_field = value.clone();
                    table.all_connection_field = None;
                }
            }
        }
        for op in &entity.omit_operations {
            let ops = &mut table.operations;
            match op {
                Operation::Read => ops.read = false,
                Operation::List => ops.list = false,
                Operation::Connection => ops.connection = false,
                // Omitting a verb omits its bulk form too, and upserts update rows
                Operation::Create => {
                    ops.create = false;
                    ops.create_many = false;
                }
                Operation::Update => {
                    ops.update = false;
                    ops.update_many = false;
                    ops.upsert = false;
                }
                Operation::Delete => {
                    ops.delete = false;
                    ops.delete_many = false;
                }
                Operation::CreateMany => ops.create_many = false,
                Operation::UpdateMany => ops.update_many = false,
                Operation::DeleteMany => ops.delete_many = false,
                Operation::Upsert => ops.upsert = false,
            }
        }
        for (col_name, col) in &entity.columns {
            let col_path = format!("{path}.columns.{col_name}");
            let Some(column) = table.columns.iter_mut().find(|c| &c.name == col_name) else {
                errors.push(format!("{col_path}: `{name}` has no column `{col_name}`"));
                continue;
            };
            if let Some(field_name) = &col.field_name {
                if check_name(&mut errors, &format!("{col_path}.field_name"), field_name) {
                    column.field_name = field_name.clone();
                }
            }
            if let Some(description) = &col.description {
                column.description = Some(description.clone());
            }
            if let Some(reason) = &col.deprecated {
                column.deprecation = Some(reason.clone());
            }
            column.omit |= col.omit;
        }
        for (fk_name, fk) in &entity.foreign_keys {
            let fk_path = format!("{path}.foreign_keys.{fk_name}");
            // Missing virtual keys were reported by `apply_structure`
            let Some(meta) = table.foreign_keys.iter_mut().find(|f| &f.constraint_name == fk_name) else { continue };
            if let Some(field_name) = &fk.field_name {
                if check_name(&mut errors, &format!("{fk_path}.field_name"), field_name) {
                    meta.field_name = Some(field_name.clone());
                }
            }
            if let Some(reverse) = &fk.reverse_field_name {
                if check_name(&mut errors, &format!("{fk_path}.reverse_field_name"), reverse) {
                    meta.reverse_field_name = Some(reverse.clone());
                }
            }
            meta.omit |= fk.omit;
            meta.omit_reverse |= fk.omit_reverse;
        }
    }

    for (name, function) in &config.functions {
        let path = format!("functions.{name}");
        let idx = function_index(catalog, config, name).ok();
        if let Some(func) = idx.map(|i| &mut catalog.functions[i]).filter(|f| f.row_argument) {
            // A computed column: `field_name` and `omit` are about its field on the table's type
            if let Some(field_name) = &function.field_name {
                if check_name(&mut errors, &format!("{path}.field_name"), field_name) {
                    func.mounted_field_name = field_name.clone();
                }
            }
            if let Some(description) = &function.description {
                func.description = Some(description.clone());
            }
            if let Some(reason) = &function.deprecated {
                func.deprecation = Some(reason.clone());
            }
            if function.omit {
                func.parent_table = None;
                func.parent_schema = None;
            }
            continue;
        }
        if let Some(func) = idx.map(|i| &mut catalog.functions[i]) {
            if let Some(field_name) = &function.field_name {
                if check_name(&mut errors, &format!("{path}.field_name"), field_name) {
                    func.field_name = field_name.clone();
                }
            }
            if let Some(description) = &function.description {
                func.description = Some(description.clone());
            }
            if let Some(reason) = &function.deprecated {
                func.deprecation = Some(reason.clone());
            }
            func.omit_root |= function.omit;
            if let Some(field_name) = function.mount.as_ref().and_then(|m| m.field_name.as_ref()) {
                if func.parent_table.is_some() && check_name(&mut errors, &format!("{path}.mount.field_name"), field_name) {
                    func.mounted_field_name = field_name.clone();
                }
            }
            continue;
        }
        // Mount errors for unknown functions were reported by `apply_structure`
        if function.mount.is_none() {
            if let Err(err) = function_index(catalog, config, name) {
                errors.push(format!("{path}: {err}"));
            }
        }
    }

    for (name, ty) in &config.types {
        let path = format!("types.{name}");
        if let Some(e) = catalog.enums.iter_mut().find(|e| &e.config_name == name) {
            if let Some(type_name) = &ty.type_name {
                if check_name(&mut errors, &format!("{path}.type_name"), type_name) {
                    e.type_name = type_name.clone();
                }
            }
            if let Some(description) = &ty.description {
                e.description = Some(description.clone());
            }
            for (label, value) in &ty.values {
                let Some(v) = e.values.iter_mut().find(|v| &v.label == label) else {
                    errors.push(format!("{path}.values: `{name}` has no value `{label}`"));
                    continue;
                };
                if check_name(&mut errors, &format!("{path}.values.{label}"), value) {
                    v.name = value.clone();
                }
            }
            let mut seen = std::collections::HashSet::new();
            for v in &e.values {
                if !seen.insert(&v.name) {
                    errors.push(format!("{path}.values: two values are named `{}`", v.name));
                }
            }
            if !ty.attributes.is_empty() {
                errors.push(format!("{path}.attributes: `{name}` is an enum; attributes apply to composite types"));
            }
        } else if let Some(c) = catalog.composites.iter_mut().find(|c| &c.config_name == name) {
            if let Some(type_name) = &ty.type_name {
                if check_name(&mut errors, &format!("{path}.type_name"), type_name) {
                    c.type_name = type_name.clone();
                }
            }
            if let Some(description) = &ty.description {
                c.description = Some(description.clone());
            }
            for (attr_name, attr) in &ty.attributes {
                let Some(a) = c.attributes.iter_mut().find(|a| &a.name == attr_name) else {
                    errors.push(format!("{path}.attributes: `{name}` has no attribute `{attr_name}`"));
                    continue;
                };
                if let Some(field_name) = &attr.field_name {
                    if check_name(&mut errors, &format!("{path}.attributes.{attr_name}.field_name"), field_name) {
                        a.field_name = field_name.clone();
                    }
                }
                if let Some(description) = &attr.description {
                    a.description = Some(description.clone());
                }
            }
            if !ty.values.is_empty() {
                errors.push(format!("{path}.values: `{name}` is a composite type; values apply to enums"));
            }
        } else if let Some(d) = catalog.domains.iter_mut().find(|d| &d.config_name == name) {
            if let Some(type_name) = &ty.type_name {
                if check_name(&mut errors, &format!("{path}.type_name"), type_name) {
                    d.type_name = type_name.clone();
                }
            }
            if let Some(description) = &ty.description {
                d.description = Some(description.clone());
            }
            if !ty.values.is_empty() || !ty.attributes.is_empty() {
                errors.push(format!("{path}: `{name}` is a domain; values and attributes apply to enums and composite types"));
            }
        } else {
            errors.push(format!(
                "{path}: no enum, composite type or domain `{name}` used by the schema (CHECK-closed columns are `<table>.<column>`)"
            ));
        }
    }

    for (schema, table) in omitted {
        catalog.remove_table(&schema, &table);
    }
    catalog.invalidate_index();
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
