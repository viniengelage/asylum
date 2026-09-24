//! Example bodies generated from schemas, and validation of bodies against them.

use crate::spec::{Spec, SpecFormat, deref};
use serde_json::{Map, Value, json};

const MAX_EXAMPLE_DEPTH: usize = 8;
/// A response with more problems than this is shown as "and N more".
const MAX_REPORTED_ISSUES: usize = 20;

/// A value that satisfies the schema as far as a placeholder can: examples and defaults
/// first, then the type's zero value.
pub fn example(document: &Value, schema: &Value) -> Value {
    example_at(document, schema, 0)
}

fn example_at(document: &Value, schema: &Value, depth: usize) -> Value {
    let schema = deref(document, schema);
    if depth > MAX_EXAMPLE_DEPTH {
        return Value::Null;
    }
    for key in ["example", "default"] {
        if let Some(value) = schema.get(key) {
            return value.clone();
        }
    }
    if let Some(first) = schema
        .get("examples")
        .and_then(Value::as_array)
        .and_then(|examples| examples.first())
    {
        return first.clone();
    }
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return first.clone();
    }
    if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
        let mut merged = Map::new();
        for part in parts {
            if let Value::Object(object) = example_at(document, part, depth + 1) {
                merged.extend(object);
            }
        }
        if !merged.is_empty() || schema.get("properties").is_none() {
            if let Value::Object(own) = object_example(document, schema, depth) {
                merged.extend(own);
            }
            return Value::Object(merged);
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(first) = schema
            .get(key)
            .and_then(Value::as_array)
            .and_then(|options| options.first())
        {
            return example_at(document, first, depth + 1);
        }
    }

    match schema_type(schema) {
        Some("object") => object_example(document, schema, depth),
        Some("array") => match schema.get("items") {
            Some(items) => Value::Array(vec![example_at(document, items, depth + 1)]),
            None => json!([]),
        },
        Some("integer") => json!(0),
        Some("number") => json!(0.0),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        Some("string") => string_example(schema),
        _ if schema.get("properties").is_some() => object_example(document, schema, depth),
        _ => Value::Null,
    }
}

fn schema_type(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(kind) => Some(kind),
        // OpenAPI 3.1 allows `type: [string, "null"]`; the first non-null type wins.
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null"),
        _ => None,
    }
}

fn object_example(document: &Value, schema: &Value, depth: usize) -> Value {
    let mut object = Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let property_schema = deref(document, property);
            if property_schema.get("readOnly").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            object.insert(name.clone(), example_at(document, property, depth + 1));
        }
    }
    Value::Object(object)
}

fn string_example(schema: &Value) -> Value {
    let example = match schema.get("format").and_then(Value::as_str) {
        Some("date-time") => "2026-01-01T12:00:00Z",
        Some("date") => "2026-01-01",
        Some("time") => "12:00:00",
        Some("email") => "pessoa@exemplo.com",
        Some("uuid") => "00000000-0000-4000-8000-000000000000",
        Some("uri") | Some("url") => "https://exemplo.com",
        Some("password") => "",
        _ => "",
    };
    Value::String(example.to_string())
}

/// The properties of an object schema, following `$ref` and `allOf`.
pub fn property_names(document: &Value, schema: &Value) -> Vec<String> {
    let schema = deref(document, schema);
    let mut names = Vec::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        names.extend(properties.keys().cloned());
    }
    if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
        for part in parts {
            for name in property_names(document, part) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// Paths (`a.b`) to every property of a schema, down to a few levels, for guessing where a
/// login response keeps its token.
pub fn property_paths(document: &Value, schema: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    collect_property_paths(document, schema, "", 0, &mut paths);
    paths
}

fn collect_property_paths(
    document: &Value,
    schema: &Value,
    prefix: &str,
    depth: usize,
    paths: &mut Vec<String>,
) {
    if depth > 3 {
        return;
    }
    let resolved = deref(document, schema);
    let mut properties: Vec<(String, Value)> = Vec::new();
    if let Some(own) = resolved.get("properties").and_then(Value::as_object) {
        properties.extend(
            own.iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    if let Some(parts) = resolved.get("allOf").and_then(Value::as_array) {
        for part in parts {
            let part = deref(document, part);
            if let Some(own) = part.get("properties").and_then(Value::as_object) {
                properties.extend(
                    own.iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
            }
        }
    }
    for (name, property) in properties {
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        paths.push(path.clone());
        collect_property_paths(document, &property, &path, depth + 1, paths);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    /// Where in the body, as a JSON pointer (`/user/id`); empty for the root.
    pub location: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Validation {
    Valid,
    Invalid {
        issues: Vec<Issue>,
        more: usize,
    },
    /// The schema itself could not be compiled (usually a regex only JavaScript accepts).
    Unchecked {
        reason: String,
    },
}

/// The document's named schemas converted to JSON Schema, so `$ref`s inside a schema keep
/// resolving once it's moved next to them.
pub fn validation_root(spec: &Spec) -> Map<String, Value> {
    let mut root = Map::new();
    match spec.format {
        SpecFormat::Swagger2 => {
            if let Some(definitions) = spec.document.get("definitions") {
                root.insert(
                    "definitions".to_string(),
                    to_json_schema(definitions, spec.format),
                );
            }
        }
        SpecFormat::OpenApi30 | SpecFormat::OpenApi31 => {
            if let Some(schemas) = spec
                .document
                .get("components")
                .and_then(|components| components.get("schemas"))
            {
                root.insert(
                    "components".to_string(),
                    json!({ "schemas": to_json_schema(schemas, spec.format) }),
                );
            }
        }
    }
    root
}

pub fn validate(
    root: &Map<String, Value>,
    format: SpecFormat,
    schema: &Value,
    instance: &Value,
) -> Validation {
    let mut combined = match to_json_schema(schema, format) {
        Value::Object(object) => object,
        Value::Bool(true) => return Validation::Valid,
        other => {
            let mut object = Map::new();
            object.insert("allOf".to_string(), json!([other]));
            object
        }
    };
    for (key, value) in root {
        combined.entry(key.clone()).or_insert_with(|| value.clone());
    }
    let validator = match jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&Value::Object(combined))
    {
        Ok(validator) => validator,
        Err(error) => {
            return Validation::Unchecked {
                reason: error.to_string(),
            };
        }
    };
    let mut issues = Vec::new();
    let mut total = 0;
    for error in validator.iter_errors(instance) {
        total += 1;
        if issues.len() < MAX_REPORTED_ISSUES {
            issues.push(Issue {
                location: error.instance_path().to_string(),
                message: error.to_string(),
            });
        }
    }
    if issues.is_empty() {
        Validation::Valid
    } else {
        Validation::Invalid {
            more: total - issues.len(),
            issues,
        }
    }
}

/// OpenAPI 3.0 and Swagger 2 schemas are a dialect of JSON Schema: `nullable`, boolean
/// `exclusiveMinimum` and a few annotation keywords need rewriting before validating.
pub fn to_json_schema(schema: &Value, format: SpecFormat) -> Value {
    if format == SpecFormat::OpenApi31 {
        return schema.clone();
    }
    convert(schema)
}

fn convert(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut converted: Map<String, Value> = object
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "nullable" | "discriminator" | "xml" | "externalDocs" | "example"
                    )
                })
                .map(|(key, value)| (key.clone(), convert(value)))
                .collect();

            if object.get("nullable").and_then(Value::as_bool) == Some(true) {
                match converted.get("type").cloned() {
                    Some(Value::String(kind)) => {
                        converted.insert("type".to_string(), json!([kind, "null"]));
                    }
                    Some(Value::Array(mut kinds)) => {
                        if !kinds.contains(&json!("null")) {
                            kinds.push(json!("null"));
                        }
                        converted.insert("type".to_string(), Value::Array(kinds));
                    }
                    _ => {}
                }
                if let Some(Value::Array(values)) = converted.get_mut("enum")
                    && !values.contains(&Value::Null)
                {
                    values.push(Value::Null);
                }
                if converted.contains_key("$ref") {
                    let reference = converted.remove("$ref").unwrap_or(Value::Null);
                    converted.insert(
                        "anyOf".to_string(),
                        json!([{ "$ref": reference }, { "type": "null" }]),
                    );
                }
            }

            for (flag, bound) in [
                ("exclusiveMinimum", "minimum"),
                ("exclusiveMaximum", "maximum"),
            ] {
                if let Some(Value::Bool(exclusive)) = object.get(flag) {
                    converted.remove(flag);
                    if *exclusive && let Some(limit) = converted.remove(bound) {
                        converted.insert(flag.to_string(), limit);
                    }
                }
            }
            Value::Object(converted)
        }
        Value::Array(items) => Value::Array(items.iter().map(convert).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;
    use pretty_assertions::assert_eq;
    use std::path::Path;

    const DOCUMENT: &str = r##"
openapi: 3.0.3
info: { title: T, version: "1" }
paths: {}
components:
  schemas:
    User:
      type: object
      required: [id, name]
      properties:
        id: { type: integer, readOnly: true }
        name: { type: string }
        email: { type: string, format: email }
        manager: { $ref: '#/components/schemas/Person', nullable: true }
        roles: { type: array, items: { type: string, enum: [admin, investor] } }
        nickname: { type: string, nullable: true }
    Person: { type: object, properties: { name: { type: string } } }
"##;

    fn spec() -> Spec {
        spec::load(Path::new("/openapi.yml"), DOCUMENT, &|_| {
            Err(anyhow::anyhow!("sem arquivos"))
        })
        .unwrap()
    }

    #[test]
    fn builds_examples_from_schemas() {
        let spec = spec();
        let example = example(
            &spec.document,
            &json!({ "$ref": "#/components/schemas/User" }),
        );
        assert_eq!(
            example,
            json!({
                "name": "",
                "email": "pessoa@exemplo.com",
                "manager": { "name": "" },
                "roles": ["admin"],
                "nickname": ""
            })
        );
    }

    #[test]
    fn validates_against_openapi_30_schemas() {
        let spec = spec();
        let root = validation_root(&spec);
        let schema = json!({ "$ref": "#/components/schemas/User" });

        let valid = json!({ "id": 1, "name": "Ana", "manager": null, "nickname": null });
        assert_eq!(
            validate(&root, spec.format, &schema, &valid),
            Validation::Valid
        );

        let invalid = json!({ "id": "1", "roles": ["root"] });
        let Validation::Invalid { issues, .. } = validate(&root, spec.format, &schema, &invalid)
        else {
            panic!("esperava problemas");
        };
        let locations: Vec<_> = issues.iter().map(|issue| issue.location.as_str()).collect();
        assert!(locations.contains(&""));
        assert!(locations.contains(&"/id"));
        assert!(locations.contains(&"/roles/0"));
    }

    #[test]
    fn lists_property_paths() {
        let spec = spec();
        let paths = property_paths(
            &spec.document,
            &json!({ "$ref": "#/components/schemas/User" }),
        );
        assert!(paths.contains(&"manager.name".to_string()));
    }
}
