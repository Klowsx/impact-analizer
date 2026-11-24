use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LaineError, Result};
use crate::model::{ContractChange, EndpointId, HttpMethod};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSchema {
    pub field_type: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndpointContract {
    /// HTTP status code (e.g. "200") -> flattened field path (e.g. "fullName", "address.city") -> schema.
    pub responses: BTreeMap<String, BTreeMap<String, FieldSchema>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiContract {
    /// Ordered map guarantees deterministic diff output across runs.
    pub endpoints: BTreeMap<EndpointId, EndpointContract>,
}

/// Normalizes an OpenAPI path template such as `/customers/{id}/` into `/customers/:id`.
pub fn normalize_openapi_path(raw: &str) -> String {
    let trimmed = raw.trim();
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2 {
                format!(":{}", &segment[1..segment.len() - 1])
            } else {
                segment.to_string()
            }
        })
        .collect();

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Converts a normalized route path like `/customers/:id` into a wildcard key `/customers/*`
/// so endpoints match even when producer and consumer use different parameter names.
pub fn canonical_route_key(normalized_path: &str) -> String {
    let segments: Vec<&str> = normalized_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.starts_with(':') || (segment.starts_with('{') && segment.ends_with('}')) {
                "*"
            } else {
                segment
            }
        })
        .collect();

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Parses an OpenAPI 3.0/3.1 document (YAML or JSON) and extracts its endpoints and JSON response fields.
pub fn extract_contract(service: &str, raw_spec: &str) -> Result<ApiContract> {
    let root: Value = serde_yaml_ng::from_str(raw_spec)
        .map_err(|err| LaineError::OpenApiParse(err.to_string()))?;

    let paths_obj = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| LaineError::OpenApiParse("missing top-level `paths` object".into()))?;

    let mut endpoints = BTreeMap::new();

    for (raw_path, path_item) in paths_obj {
        let Some(path_map) = path_item.as_object() else {
            continue;
        };

        let normalized_path = normalize_openapi_path(raw_path);

        for (method_key, operation) in path_map {
            let Some(method) = HttpMethod::parse(method_key) else {
                continue;
            };

            let mut endpoint_contract = EndpointContract::default();

            if let Some(responses) = operation.get("responses").and_then(Value::as_object) {
                for (status_code, response_val) in responses {
                    if !status_code.starts_with('2') {
                        continue;
                    }

                    let mut visited = HashSet::new();
                    let resolved_response = resolve_ref(&root, response_val, &mut visited);

                    if let Some(schema) = extract_json_schema(resolved_response) {
                        let mut fields = BTreeMap::new();
                        collect_schema_fields(&root, schema, "", &mut visited, &mut fields);
                        endpoint_contract
                            .responses
                            .insert(status_code.clone(), fields);
                    }
                }
            }

            let endpoint_id = EndpointId {
                service: service.to_string(),
                method,
                normalized_path: normalized_path.clone(),
            };
            endpoints.insert(endpoint_id, endpoint_contract);
        }
    }

    Ok(ApiContract { endpoints })
}

/// Compares `base` (before) against `head` (after) and returns breaking contract changes.
///
/// Additive changes (new endpoints, new response fields) are considered backwards-compatible
/// and are intentionally not reported as breaking changes.
pub fn diff_contracts(base: &ApiContract, head: &ApiContract) -> Vec<ContractChange> {
    let mut changes = Vec::new();

    for (endpoint_id, base_endpoint) in &base.endpoints {
        let Some(head_endpoint) = find_matching_endpoint(head, endpoint_id) else {
            changes.push(ContractChange::EndpointRemoved {
                endpoint: endpoint_id.clone(),
            });
            continue;
        };

        for (status_code, base_fields) in &base_endpoint.responses {
            let head_fields = head_endpoint.responses.get(status_code);

            for (field_path, base_field) in base_fields {
                match head_fields.and_then(|fields| fields.get(field_path)) {
                    None => {
                        changes.push(ContractChange::ResponseFieldRemoved {
                            endpoint: endpoint_id.clone(),
                            status_code: status_code.clone(),
                            field_path: field_path.clone(),
                            previous_type: base_field.field_type.clone(),
                        });
                    }
                    Some(head_field) if head_field.field_type != base_field.field_type => {
                        changes.push(ContractChange::ResponseFieldTypeChanged {
                            endpoint: endpoint_id.clone(),
                            status_code: status_code.clone(),
                            field_path: field_path.clone(),
                            previous_type: base_field.field_type.clone(),
                            new_type: head_field.field_type.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
        }
    }

    changes
}

/// Best-effort 1-based line lookup in the raw OpenAPI file for reporting producer evidence.
pub fn find_field_line(raw_spec: &str, field_name: &str) -> usize {
    let leaf = field_name.rsplit('.').next().unwrap_or(field_name);
    let yaml_key = format!("{leaf}:");
    let json_key = format!("\"{leaf}\"");

    raw_spec
        .lines()
        .enumerate()
        .find_map(|(idx, line)| {
            let trimmed = line.trim_start();
            if trimmed.starts_with(&yaml_key) || trimmed.starts_with(&json_key) {
                Some(idx + 1)
            } else {
                None
            }
        })
        .unwrap_or(1)
}

fn find_matching_endpoint<'a>(
    contract: &'a ApiContract,
    target: &EndpointId,
) -> Option<&'a EndpointContract> {
    if let Some(exact) = contract.endpoints.get(target) {
        return Some(exact);
    }

    let target_key = canonical_route_key(&target.normalized_path);
    contract.endpoints.iter().find_map(|(id, ep)| {
        if id.service == target.service
            && id.method == target.method
            && canonical_route_key(&id.normalized_path) == target_key
        {
            Some(ep)
        } else {
            None
        }
    })
}

fn extract_json_schema(response: &Value) -> Option<&Value> {
    let content = response.get("content")?.as_object()?;
    let media = content.get("application/json").or_else(|| {
        content
            .iter()
            .find(|(mime, _)| mime.contains("json"))
            .map(|(_, val)| val)
    })?;
    media.get("schema")
}

fn resolve_ref<'a>(root: &'a Value, node: &'a Value, visited: &mut HashSet<String>) -> &'a Value {
    let Some(ref_str) = node.get("$ref").and_then(Value::as_str) else {
        return node;
    };

    if !ref_str.starts_with("#/") || !visited.insert(ref_str.to_string()) {
        return node;
    }

    let mut current = root;
    for part in ref_str.trim_start_matches("#/").split('/') {
        let unescaped = part.replace("~1", "/").replace("~0", "~");
        match current.get(&unescaped) {
            Some(next) => current = next,
            None => return node,
        }
    }

    resolve_ref(root, current, visited)
}

fn collect_schema_fields(
    root: &Value,
    schema: &Value,
    prefix: &str,
    visited: &mut HashSet<String>,
    out: &mut BTreeMap<String, FieldSchema>,
) {
    let mut local_visited = visited.clone();
    let resolved = resolve_ref(root, schema, &mut local_visited);

    // Support `allOf` composition by merging sub-schemas.
    if let Some(all_of) = resolved.get("allOf").and_then(Value::as_array) {
        for sub in all_of {
            collect_schema_fields(root, sub, prefix, &mut local_visited, out);
        }
    }

    let required_set: HashSet<&str> = resolved
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    if let Some(props) = resolved.get("properties").and_then(Value::as_object) {
        for (prop_name, prop_schema) in props {
            let mut prop_visited = local_visited.clone();
            let resolved_prop = resolve_ref(root, prop_schema, &mut prop_visited);

            let full_path = if prefix.is_empty() {
                prop_name.clone()
            } else {
                format!("{prefix}.{prop_name}")
            };

            let field_type = extract_type_label(resolved_prop);
            out.insert(
                full_path.clone(),
                FieldSchema {
                    field_type,
                    required: required_set.contains(prop_name.as_str()),
                },
            );

            // Recurse into nested objects and arrays of objects.
            if resolved_prop.get("properties").is_some() || resolved_prop.get("allOf").is_some() {
                collect_schema_fields(root, resolved_prop, &full_path, &mut prop_visited, out);
            } else if let Some(items) = resolved_prop.get("items") {
                let array_prefix = format!("{full_path}[]");
                collect_schema_fields(root, items, &array_prefix, &mut prop_visited, out);
            }
        }
    } else if let Some(items) = resolved.get("items") {
        let array_prefix = if prefix.is_empty() {
            "[]".to_string()
        } else {
            format!("{prefix}[]")
        };
        collect_schema_fields(root, items, &array_prefix, &mut local_visited, out);
    }
}

fn extract_type_label(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(ty)) => ty.clone(),
        Some(Value::Array(arr)) => {
            let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
            if parts.is_empty() {
                "unknown".to_string()
            } else {
                parts.join(" | ")
            }
        }
        _ if schema.get("properties").is_some() => "object".to_string(),
        _ if schema.get("items").is_some() => "array".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::fixtures::create_demo_workspace;
    use crate::git::GitRepo;

    #[test]
    fn normalizes_paths_and_canonical_keys() {
        assert_eq!(normalize_openapi_path("/customers/{id}"), "/customers/:id");
        assert_eq!(normalize_openapi_path("/customers/{id}/"), "/customers/:id");
        assert_eq!(canonical_route_key("/customers/:id"), "/customers/*");
        assert_eq!(
            canonical_route_key("/customers/:customerId"),
            "/customers/*"
        );
    }

    #[test]
    fn resolves_local_refs_and_detects_field_removal_and_type_change() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /customers/{id}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Customer'
  /legacy/ping:
    get:
      responses:
        '200':
          description: ok
components:
  schemas:
    Customer:
      type: object
      required: [id, fullName]
      properties:
        id:
          type: integer
        fullName:
          type: string
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /customers/{customerId}:
    get:
      responses:
        '200':
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Customer'
components:
  schemas:
    Customer:
      type: object
      required: [id, firstName]
      properties:
        id:
          type: string
        firstName:
          type: string
"#;

        let base = extract_contract("customer-service", base_yaml).unwrap();
        let head = extract_contract("customer-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        assert_eq!(
            changes,
            vec![
                ContractChange::ResponseFieldRemoved {
                    endpoint: EndpointId {
                        service: "customer-service".into(),
                        method: HttpMethod::Get,
                        normalized_path: "/customers/:id".into(),
                    },
                    status_code: "200".into(),
                    field_path: "fullName".into(),
                    previous_type: "string".into(),
                },
                ContractChange::ResponseFieldTypeChanged {
                    endpoint: EndpointId {
                        service: "customer-service".into(),
                        method: HttpMethod::Get,
                        normalized_path: "/customers/:id".into(),
                    },
                    status_code: "200".into(),
                    field_path: "id".into(),
                    previous_type: "integer".into(),
                    new_type: "string".into(),
                },
                ContractChange::EndpointRemoved {
                    endpoint: EndpointId {
                        service: "customer-service".into(),
                        method: HttpMethod::Get,
                        normalized_path: "/legacy/ping".into(),
                    },
                },
            ]
        );
    }

    #[test]
    fn detects_fullname_removal_across_real_git_commits_in_demo_fixture() {
        let tmp =
            std::env::temp_dir().join(format!("laine-openapi-git-test-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");

        let repo = GitRepo::new(&paths.customer_service);
        let base_spec = repo
            .read_file("main", Path::new("openapi.yaml"))
            .expect("main openapi should exist");
        let head_spec = repo
            .read_file("feature/split-customer-name", Path::new("openapi.yaml"))
            .expect("head openapi should exist");

        let base_contract = extract_contract("customer-service", &base_spec).unwrap();
        let head_contract = extract_contract("customer-service", &head_spec).unwrap();
        let changes = diff_contracts(&base_contract, &head_contract);

        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0],
            ContractChange::ResponseFieldRemoved {
                endpoint: EndpointId {
                    service: "customer-service".into(),
                    method: HttpMethod::Get,
                    normalized_path: "/customers/:id".into(),
                },
                status_code: "200".into(),
                field_path: "fullName".into(),
                previous_type: "string".into(),
            }
        );

        let line = find_field_line(&base_spec, "fullName");
        assert!(line > 1);

        let _ = fs::remove_dir_all(&tmp);
    }
}
