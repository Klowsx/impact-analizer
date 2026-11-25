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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterSchema {
    pub param_type: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndpointContract {
    /// HTTP status code (e.g. "200") -> flattened field path (e.g. "fullName", "address.city") -> schema.
    pub responses: BTreeMap<String, BTreeMap<String, FieldSchema>>,
    /// Flattened JSON request body field path -> schema.
    #[serde(default)]
    pub request_fields: BTreeMap<String, FieldSchema>,
    /// Parameter keyed by `(location, name)` (e.g. `("query", "tenantId")`) -> schema.
    #[serde(default)]
    pub parameters: BTreeMap<(String, String), ParameterSchema>,
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
/// (and optional params like `/corp/:accion/:param?` into `/corp/*/*?`) so endpoints match
/// even when producer and consumer use different parameter names or omit optional segments.
pub fn canonical_route_key(normalized_path: &str) -> String {
    let segments: Vec<&str> = normalized_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.starts_with(':') {
                if segment.ends_with('?') { "*?" } else { "*" }
            } else if segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2 {
                let inner = &segment[1..segment.len() - 1];
                if inner.ends_with('?') { "*?" } else { "*" }
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

/// Parses an OpenAPI 3.0/3.1 document (YAML or JSON) and extracts its endpoints,
/// JSON response fields, JSON request body fields, and parameters.
pub fn extract_contract(service: &str, raw_spec: &str) -> Result<ApiContract> {
    let root: Value = serde_yaml_ng::from_str(raw_spec)
        .map_err(|err| LaineError::OpenApiParse(err.to_string()))?;

    let paths_obj = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| LaineError::OpenApiParse("missing top-level `paths` object".into()))?;

    let mut endpoints = BTreeMap::new();

    for (raw_path, path_item) in paths_obj {
        let mut path_visited = HashSet::new();
        let resolved_path_item = resolve_ref(&root, path_item, &mut path_visited);
        let Some(path_map) = resolved_path_item.as_object() else {
            continue;
        };

        let normalized_path = normalize_openapi_path(raw_path);
        let mut path_level_params = BTreeMap::new();
        if let Some(params) = resolved_path_item
            .get("parameters")
            .and_then(Value::as_array)
        {
            collect_parameters(&root, params, &mut path_level_params);
        }

        for (method_key, operation) in path_map {
            let Some(method) = HttpMethod::parse(method_key) else {
                continue;
            };

            let mut endpoint_contract = EndpointContract {
                parameters: path_level_params.clone(),
                ..EndpointContract::default()
            };

            if let Some(params) = operation.get("parameters").and_then(Value::as_array) {
                collect_parameters(&root, params, &mut endpoint_contract.parameters);
            }

            if let Some(request_body_val) = operation.get("requestBody") {
                let mut visited = HashSet::new();
                let resolved_body = resolve_ref(&root, request_body_val, &mut visited);

                if let Some(schema) = extract_json_schema(resolved_body) {
                    collect_schema_fields(
                        &root,
                        schema,
                        "",
                        &mut visited,
                        &mut endpoint_contract.request_fields,
                    );
                }
            }

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
/// Additive backwards-compatible changes (new endpoints, new response fields, new optional
/// request fields or optional parameters) are intentionally not reported as breaking changes.
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

        for (field_path, head_field) in &head_endpoint.request_fields {
            match base_endpoint.request_fields.get(field_path) {
                None => {
                    if head_field.required {
                        changes.push(ContractChange::RequestFieldAddedRequired {
                            endpoint: endpoint_id.clone(),
                            field_path: field_path.clone(),
                            field_type: head_field.field_type.clone(),
                        });
                    }
                }
                Some(base_field) => {
                    if head_field.required && !base_field.required {
                        changes.push(ContractChange::RequestFieldAddedRequired {
                            endpoint: endpoint_id.clone(),
                            field_path: field_path.clone(),
                            field_type: head_field.field_type.clone(),
                        });
                    }
                    if base_field.field_type != head_field.field_type {
                        changes.push(ContractChange::RequestFieldTypeChanged {
                            endpoint: endpoint_id.clone(),
                            field_path: field_path.clone(),
                            previous_type: base_field.field_type.clone(),
                            new_type: head_field.field_type.clone(),
                        });
                    }
                }
            }
        }

        for ((location, name), head_param) in &head_endpoint.parameters {
            let was_required = base_endpoint
                .parameters
                .get(&(location.clone(), name.clone()))
                .is_some_and(|p| p.required);
            if head_param.required && !was_required {
                changes.push(ContractChange::RequiredParameterAdded {
                    endpoint: endpoint_id.clone(),
                    location: location.clone(),
                    name: name.clone(),
                    param_type: head_param.param_type.clone(),
                });
            }
        }
    }

    changes
}

/// Best-effort 1-based line lookup in the raw OpenAPI file for reporting producer evidence.
pub fn find_field_line(raw_spec: &str, field_name: &str) -> usize {
    let leaf = field_name
        .rsplit('.')
        .next()
        .unwrap_or(field_name)
        .trim_end_matches("[]");
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

/// Best-effort 1-based line lookup for an OpenAPI parameter definition (`name: <param>`).
pub fn find_parameter_line(raw_spec: &str, param_name: &str) -> usize {
    let yaml_plain = format!("name: {param_name}");
    let yaml_double = format!("name: \"{param_name}\"");
    let yaml_single = format!("name: '{param_name}'");
    let json_val = format!("\"name\": \"{param_name}\"");

    raw_spec
        .lines()
        .enumerate()
        .find_map(|(idx, line)| {
            let trimmed = line.trim_start();
            let normalized = trimmed
                .strip_prefix("- ")
                .map(str::trim_start)
                .unwrap_or(trimmed);
            if normalized == yaml_plain
                || normalized == yaml_double
                || normalized == yaml_single
                || normalized.starts_with(&json_val)
            {
                Some(idx + 1)
            } else {
                None
            }
        })
        .unwrap_or_else(|| find_field_line(raw_spec, param_name))
}

fn collect_parameters(
    root: &Value,
    params: &[Value],
    out: &mut BTreeMap<(String, String), ParameterSchema>,
) {
    for param_val in params {
        let mut visited = HashSet::new();
        let resolved = resolve_ref(root, param_val, &mut visited);

        let Some(location) = resolved.get("in").and_then(Value::as_str) else {
            continue;
        };
        let Some(name) = resolved.get("name").and_then(Value::as_str) else {
            continue;
        };

        let required = location == "path"
            || resolved
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);

        let param_type = resolved
            .get("schema")
            .or_else(|| extract_json_schema(resolved))
            .map(|schema| {
                let mut schema_visited = visited.clone();
                let resolved_schema = resolve_ref(root, schema, &mut schema_visited);
                extract_type_label(resolved_schema)
            })
            .unwrap_or_else(|| "unknown".to_string());

        out.insert(
            (location.to_string(), name.to_string()),
            ParameterSchema {
                param_type,
                required,
            },
        );
    }
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
        // Apply `required` constraints declared in any `allOf` branch to sibling-defined fields.
        for sub in all_of {
            let mut sub_visited = local_visited.clone();
            let resolved_sub = resolve_ref(root, sub, &mut sub_visited);
            apply_required_flags(resolved_sub, prefix, out);
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
            let is_required = required_set.contains(prop_name.as_str());
            out.entry(full_path.clone())
                .and_modify(|existing| {
                    existing.field_type = field_type.clone();
                    existing.required |= is_required;
                })
                .or_insert(FieldSchema {
                    field_type,
                    required: is_required,
                });

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

    apply_required_flags(resolved, prefix, out);
}

fn apply_required_flags(schema: &Value, prefix: &str, out: &mut BTreeMap<String, FieldSchema>) {
    if let Some(arr) = schema.get("required").and_then(Value::as_array) {
        for req_name in arr.iter().filter_map(Value::as_str) {
            let full_path = if prefix.is_empty() {
                req_name.to_string()
            } else {
                format!("{prefix}.{req_name}")
            };
            if let Some(field) = out.get_mut(&full_path) {
                field.required = true;
            }
        }
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

    #[test]
    fn detects_new_required_request_field_and_optional_to_required_transition() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    post:
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [email]
              properties:
                email:
                  type: string
                nickname:
                  type: string
      responses:
        '201':
          description: created
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    post:
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [email, nickname, taxId]
              properties:
                email:
                  type: string
                nickname:
                  type: string
                taxId:
                  type: string
      responses:
        '201':
          description: created
"#;

        let base = extract_contract("customer-service", base_yaml).unwrap();
        let head = extract_contract("customer-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        let endpoint = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Post,
            normalized_path: "/customers".into(),
        };

        assert_eq!(
            changes,
            vec![
                ContractChange::RequestFieldAddedRequired {
                    endpoint: endpoint.clone(),
                    field_path: "nickname".into(),
                    field_type: "string".into(),
                },
                ContractChange::RequestFieldAddedRequired {
                    endpoint,
                    field_path: "taxId".into(),
                    field_type: "string".into(),
                },
            ]
        );
    }

    #[test]
    fn detects_request_field_type_change_including_nested_objects() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /orders:
    post:
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                amount:
                  type: integer
                shipping:
                  type: object
                  properties:
                    postalCode:
                      type: integer
      responses:
        '200':
          description: ok
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /orders:
    post:
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                amount:
                  type: string
                shipping:
                  type: object
                  properties:
                    postalCode:
                      type: string
      responses:
        '200':
          description: ok
"#;

        let base = extract_contract("order-service", base_yaml).unwrap();
        let head = extract_contract("order-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        let endpoint = EndpointId {
            service: "order-service".into(),
            method: HttpMethod::Post,
            normalized_path: "/orders".into(),
        };

        assert_eq!(
            changes,
            vec![
                ContractChange::RequestFieldTypeChanged {
                    endpoint: endpoint.clone(),
                    field_path: "amount".into(),
                    previous_type: "integer".into(),
                    new_type: "string".into(),
                },
                ContractChange::RequestFieldTypeChanged {
                    endpoint,
                    field_path: "shipping.postalCode".into(),
                    previous_type: "integer".into(),
                    new_type: "string".into(),
                },
            ]
        );
    }

    #[test]
    fn detects_new_required_query_and_header_parameters_including_path_level_merge() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    parameters:
      - name: X-Trace-Id
        in: header
        required: false
        schema:
          type: string
    get:
      parameters:
        - name: page
          in: query
          required: false
          schema:
            type: integer
      responses:
        '200':
          description: ok
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    parameters:
      - name: X-Trace-Id
        in: header
        required: true
        schema:
          type: string
      - name: X-Region
        in: header
        required: true
        schema:
          type: string
    get:
      parameters:
        - name: X-Region
          in: header
          required: false
          schema:
            type: string
        - name: tenantId
          in: query
          required: true
          schema:
            type: string
        - name: page
          in: query
          required: true
          schema:
            type: integer
      responses:
        '200':
          description: ok
"#;

        let base = extract_contract("customer-service", base_yaml).unwrap();
        let head = extract_contract("customer-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        let endpoint = EndpointId {
            service: "customer-service".into(),
            method: HttpMethod::Get,
            normalized_path: "/customers".into(),
        };

        // Operation-level X-Region (required: false) overrides path-level (required: true).
        assert_eq!(
            changes,
            vec![
                ContractChange::RequiredParameterAdded {
                    endpoint: endpoint.clone(),
                    location: "header".into(),
                    name: "X-Trace-Id".into(),
                    param_type: "string".into(),
                },
                ContractChange::RequiredParameterAdded {
                    endpoint: endpoint.clone(),
                    location: "query".into(),
                    name: "page".into(),
                    param_type: "integer".into(),
                },
                ContractChange::RequiredParameterAdded {
                    endpoint,
                    location: "query".into(),
                    name: "tenantId".into(),
                    param_type: "string".into(),
                },
            ]
        );
    }

    #[test]
    fn resolves_refs_and_allof_in_request_body_and_parameters() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /invoices:
    post:
      parameters:
        - $ref: '#/components/parameters/OptionalTenant'
      requestBody:
        $ref: '#/components/requestBodies/CreateInvoice'
      responses:
        '201':
          description: created
components:
  parameters:
    OptionalTenant:
      name: X-Tenant-Id
      in: header
      required: false
      schema:
        $ref: '#/components/schemas/TenantId'
  requestBodies:
    CreateInvoice:
      required: true
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/InvoiceInput'
  schemas:
    TenantId:
      type: string
    BaseInvoice:
      type: object
      properties:
        currency:
          type: string
        total:
          type: number
    InvoiceInput:
      allOf:
        - $ref: '#/components/schemas/BaseInvoice'
        - type: object
          required: [total]
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /invoices:
    post:
      parameters:
        - $ref: '#/components/parameters/RequiredTenant'
      requestBody:
        $ref: '#/components/requestBodies/CreateInvoice'
      responses:
        '201':
          description: created
components:
  parameters:
    RequiredTenant:
      name: X-Tenant-Id
      in: header
      required: true
      schema:
        $ref: '#/components/schemas/TenantId'
  requestBodies:
    CreateInvoice:
      required: true
      content:
        application/json:
          schema:
            $ref: '#/components/schemas/InvoiceInput'
  schemas:
    TenantId:
      type: string
    BaseInvoice:
      type: object
      properties:
        currency:
          type: string
        total:
          type: string
    InvoiceInput:
      allOf:
        - $ref: '#/components/schemas/BaseInvoice'
        - type: object
          required: [currency, total]
"#;

        let base = extract_contract("billing-service", base_yaml).unwrap();
        let head = extract_contract("billing-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        let endpoint = EndpointId {
            service: "billing-service".into(),
            method: HttpMethod::Post,
            normalized_path: "/invoices".into(),
        };

        assert_eq!(
            changes,
            vec![
                ContractChange::RequestFieldAddedRequired {
                    endpoint: endpoint.clone(),
                    field_path: "currency".into(),
                    field_type: "string".into(),
                },
                ContractChange::RequestFieldTypeChanged {
                    endpoint: endpoint.clone(),
                    field_path: "total".into(),
                    previous_type: "number".into(),
                    new_type: "string".into(),
                },
                ContractChange::RequiredParameterAdded {
                    endpoint,
                    location: "header".into(),
                    name: "X-Tenant-Id".into(),
                    param_type: "string".into(),
                },
            ]
        );

        let param_line = find_parameter_line(head_yaml, "X-Tenant-Id");
        assert!(param_line > 1);
    }

    #[test]
    fn optional_request_fields_and_optional_parameters_are_backwards_compatible() {
        let base_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    post:
      parameters:
        - name: X-Legacy
          in: header
          required: true
          schema:
            type: string
      requestBody:
        content:
          application/json:
            schema:
              type: object
              required: [email]
              properties:
                email:
                  type: string
      responses:
        '201':
          description: created
"#;

        let head_yaml = r#"
openapi: 3.0.3
paths:
  /customers:
    post:
      parameters:
        - name: X-Legacy
          in: header
          required: false
          schema:
            type: string
        - name: dryRun
          in: query
          required: false
          schema:
            type: boolean
        - name: session
          in: cookie
          schema:
            type: string
      requestBody:
        content:
          application/json:
            schema:
              type: object
              properties:
                email:
                  type: string
                middleName:
                  type: string
                notes:
                  type: string
      responses:
        '201':
          description: created
"#;

        let base = extract_contract("customer-service", base_yaml).unwrap();
        let head = extract_contract("customer-service", head_yaml).unwrap();
        let changes = diff_contracts(&base, &head);

        assert!(
            changes.is_empty(),
            "optional additions and relaxations must produce zero breaking changes, got: {changes:?}"
        );
    }
}
