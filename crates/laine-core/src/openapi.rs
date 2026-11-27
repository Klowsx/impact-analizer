use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use saphyr::{LoadableYamlNode, MarkedYaml, Scalar, YamlData};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LaineError, Result};
use crate::git::GitRepo;
use crate::model::{ContractChange, EndpointId, ExtractionFailure, HttpMethod, SourceSpan};

#[path = "openapi_refs.rs"]
mod references;
use references::{Documents, Resolution, VisitedReferences};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractSource {
    pub file: String,
    pub pointer: String,
    pub span: Option<SourceSpan>,
}

type SourceIndex = HashMap<*const Value, ContractSource>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSchema {
    pub field_type: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conditional: bool,
    #[serde(skip)]
    source: Option<ContractSource>,
    #[serde(skip)]
    type_source: Option<ContractSource>,
    #[serde(skip)]
    required_source: Option<ContractSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterSchema {
    pub param_type: String,
    pub required: bool,
    #[serde(skip)]
    type_source: Option<ContractSource>,
    #[serde(skip)]
    source: Option<ContractSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndpointContract {
    #[serde(skip)]
    incomplete: bool,
    #[serde(skip)]
    source: Option<ContractSource>,
    /// HTTP status code (e.g. "200") -> flattened field path (e.g. "fullName", "address.city") -> schema.
    pub responses: BTreeMap<String, BTreeMap<String, FieldSchema>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response_bodies: BTreeMap<String, FieldSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<FieldSchema>,
    /// Flattened JSON request body field path -> schema.
    #[serde(default)]
    pub request_fields: BTreeMap<String, FieldSchema>,
    /// Parameter keyed by `(location, name)` (e.g. `("query", "tenantId")`) -> schema.
    #[serde(default)]
    pub parameters: BTreeMap<(String, String), ParameterSchema>,
}

impl EndpointContract {
    /// Locate the operation in the OpenAPI document that declares it.
    pub fn source(&self) -> Option<&ContractSource> {
        self.source.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApiContract {
    /// Ordered map guarantees deterministic diff output across runs.
    pub endpoints: BTreeMap<EndpointId, EndpointContract>,
    #[serde(skip)]
    incomplete_paths: HashSet<String>,
}

pub struct OpenApiExtraction {
    pub contract: ApiContract,
    pub diagnostics: Vec<(ContractSource, ExtractionFailure)>,
    pub loaded_files: usize,
}

pub struct OpenApiSnapshot {
    pub file: String,
    contents: String,
}

impl OpenApiSnapshot {
    /// Read an OpenAPI document by its repository-relative path in a single snapshot.
    pub fn read(git: &GitRepo, revision: &str, path: &str) -> Result<Self> {
        Ok(Self {
            file: path.to_owned(),
            contents: git.read_file(revision, Path::new(path))?,
        })
    }

    /// Extract the contract and resolve external references from the same snapshot.
    pub fn extract(
        &self,
        git: &GitRepo,
        revision: &str,
        service: &str,
    ) -> Result<OpenApiExtraction> {
        extract_contract_with_references(service, &self.file, &self.contents, |path| {
            git.read_file(revision, Path::new(path)).ok()
        })
    }
}

impl ApiContract {
    // Encuentra la declaración concreta usada para un cambio dentro de este snapshot.
    pub fn source_for_change(&self, change: &ContractChange) -> Option<&ContractSource> {
        let endpoint = find_matching_endpoint(self, change.endpoint())?;
        match change {
            ContractChange::EndpointRemoved { .. } => endpoint.source(),
            ContractChange::ResponseFieldRemoved {
                status_code,
                field_path,
                ..
            } => endpoint
                .responses
                .get(status_code)?
                .get(field_path)?
                .source
                .as_ref(),
            ContractChange::ResponseFieldTypeChanged {
                status_code,
                field_path,
                ..
            } => {
                let field = endpoint.responses.get(status_code)?.get(field_path)?;
                field.type_source.as_ref().or(field.source.as_ref())
            }
            ContractChange::ResponseFieldMadeOptional {
                status_code,
                field_path,
                ..
            } => {
                let field = endpoint.responses.get(status_code)?.get(field_path)?;
                field.required_source.as_ref().or(field.source.as_ref())
            }
            ContractChange::BodyTypeChanged { status_code, .. } => {
                let body = if let Some(status) = status_code {
                    endpoint.response_bodies.get(status)?
                } else {
                    endpoint.request_body.as_ref()?
                };
                body.source.as_ref()
            }
            ContractChange::RequestParameterTypeChanged { location, name, .. } => {
                let parameter = endpoint.parameters.get(&(location.clone(), name.clone()))?;
                parameter.type_source.as_ref().or(parameter.source.as_ref())
            }
            ContractChange::RequestFieldAddedRequired { field_path, .. } => {
                let field = endpoint.request_fields.get(field_path)?;
                field.required_source.as_ref().or(field.source.as_ref())
            }
            ContractChange::RequestFieldTypeChanged { field_path, .. } => {
                let field = endpoint.request_fields.get(field_path)?;
                field.type_source.as_ref().or(field.source.as_ref())
            }
            ContractChange::RequiredParameterAdded { location, name, .. } => endpoint
                .parameters
                .get(&(location.clone(), name.clone()))?
                .source
                .as_ref(),
        }
    }
}

// Asocia el árbol semántico con las posiciones de claves y elementos del documento original.
fn index_sources(root: &Value, raw: &str, file: &str) -> SourceIndex {
    let marked = MarkedYaml::load_from_str(raw.strip_prefix('\u{feff}').unwrap_or(raw)).ok();
    let mut sources = HashMap::new();
    index_source_node(
        root,
        marked.as_ref().and_then(|nodes| nodes.first()),
        None,
        "",
        file,
        &mut sources,
    );
    sources
}

// Conserva el JSON Pointer aunque el parser auxiliar no pueda ubicar el nodo.
fn index_source_node(
    node: &Value,
    marked: Option<&MarkedYaml<'_>>,
    key: Option<&MarkedYaml<'_>>,
    pointer: &str,
    file: &str,
    sources: &mut SourceIndex,
) {
    let span = key
        .or(marked)
        .map(|node| node.span)
        .filter(|span| span.start.line() > 0)
        .map(|span| SourceSpan {
            line: span.start.line(),
            column: span.start.col() + 1,
            end_line: span.end.line().max(span.start.line()),
            end_column: if span.end.line() > 0 {
                span.end.col() + 1
            } else {
                span.start.col() + 1
            },
        });
    sources.insert(
        node,
        ContractSource {
            file: file.to_owned(),
            pointer: pointer.into(),
            span,
        },
    );

    if let Some(object) = node.as_object() {
        let mapping = marked.and_then(|node| node.data.as_mapping());
        for (name, value) in object {
            let pair = mapping.and_then(|entries| {
                entries
                    .get_key_value(&MarkedYaml::from(YamlData::Value(Scalar::String(
                        name.as_str().into(),
                    ))))
                    .or_else(|| entries.get_key_value(&MarkedYaml::value_from_str(name)))
            });
            let escaped = name.replace('~', "~0").replace('/', "~1");
            index_source_node(
                value,
                pair.map(|(_, value)| value),
                pair.map(|(key, _)| key),
                &format!("{pointer}/{escaped}"),
                file,
                sources,
            );
        }
    } else if let Some(array) = node.as_array() {
        for (index, value) in array.iter().enumerate() {
            let item = marked.and_then(|node| node.data.as_sequence_get(index));
            index_source_node(
                value,
                item,
                None,
                &format!("{pointer}/{index}"),
                file,
                sources,
            );
        }
    }
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

/// Extrae las operaciones de un único documento OpenAPI YAML o JSON.
pub fn extract_contract(service: &str, raw_spec: &str) -> Result<ApiContract> {
    Ok(extract_contract_with_references(service, "", raw_spec, |_| None)?.contract)
}

// Parsea la representación estructurada de un archivo, con BOM UTF-8 opcional.
fn parse_document(raw: &str) -> Result<Value> {
    serde_yaml_ng::from_str(raw.strip_prefix('\u{feff}').unwrap_or(raw))
        .map_err(|err| LaineError::OpenApiParse(err.to_string()))
}

/// Extrae un contrato con referencias relativas desde el mismo snapshot del archivo principal.
pub fn extract_contract_with_references(
    service: &str,
    file: &str,
    raw_spec: &str,
    read: impl FnMut(&str) -> Option<String>,
) -> Result<OpenApiExtraction> {
    let documents = Documents::load(file, raw_spec, read)?;
    let root = documents.root(file);
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| LaineError::OpenApiParse("missing top-level `paths` object".into()))?;
    let uses_nullable = root
        .get("openapi")
        .and_then(Value::as_str)
        .is_some_and(|version| version.starts_with("3.0."));
    let mut contract = ApiContract::default();
    let mut diagnostics = Vec::new();

    for (raw_path, path_item) in paths {
        let normalized_path = normalize_openapi_path(raw_path);
        let mut context = documents.context();
        let mut visited = VisitedReferences::new();
        let Some(path_item) = context.follow(path_item, &mut visited) else {
            contract
                .incomplete_paths
                .insert(canonical_route_key(&normalized_path));
            diagnostics.extend(context.diagnostics);
            continue;
        };
        let Some(path_map) = path_item.as_object() else {
            continue;
        };

        let mut path_level_params = BTreeMap::new();
        if let Some(params) = path_item.get("parameters").and_then(Value::as_array) {
            collect_parameters(
                &mut context,
                params,
                &visited,
                &mut path_level_params,
                uses_nullable,
            );
        }
        let path_parameters_incomplete = !context.diagnostics.is_empty();
        diagnostics.extend(context.diagnostics);

        for (method, operation) in path_map {
            let Some(method) = HttpMethod::parse(method) else {
                continue;
            };
            let mut context = documents.context();
            let endpoint = extract_operation(
                &mut context,
                operation,
                &visited,
                path_level_params.clone(),
                uses_nullable,
                path_parameters_incomplete,
            );
            diagnostics.extend(context.diagnostics);
            contract.endpoints.insert(
                EndpointId {
                    service: service.to_owned(),
                    method,
                    normalized_path: normalized_path.clone(),
                },
                endpoint,
            );
        }
    }
    diagnostics.sort_by(|(left, a), (right, b)| {
        (&left.file, &left.pointer, a).cmp(&(&right.file, &right.pointer, b))
    });
    diagnostics.dedup();
    Ok(OpenApiExtraction {
        contract,
        diagnostics,
        loaded_files: documents.file_count(),
    })
}

// Extrae parámetros y cuerpos de una operación y registra cobertura parcial si alguna referencia falla.
fn extract_operation<'a>(
    context: &mut Resolution<'a>,
    operation: &'a Value,
    visited: &VisitedReferences,
    parameters: BTreeMap<(String, String), ParameterSchema>,
    uses_nullable: bool,
    path_parameters_incomplete: bool,
) -> EndpointContract {
    let mut endpoint = EndpointContract {
        source: context.source(operation).cloned(),
        parameters,
        ..EndpointContract::default()
    };
    if let Some(params) = operation.get("parameters").and_then(Value::as_array) {
        collect_parameters(
            context,
            params,
            visited,
            &mut endpoint.parameters,
            uses_nullable,
        );
    }
    if let Some(request) = operation.get("requestBody")
        && let Some((body, fields)) = extract_body(context, request, visited, uses_nullable)
    {
        endpoint.request_body = Some(body);
        endpoint.request_fields = fields;
    }
    if let Some(responses) = operation.get("responses").and_then(Value::as_object) {
        for (status, response) in responses {
            if status.starts_with('2')
                && let Some((body, fields)) =
                    extract_body(context, response, visited, uses_nullable)
            {
                endpoint.response_bodies.insert(status.clone(), body);
                endpoint.responses.insert(status.clone(), fields);
            }
        }
    }
    endpoint.incomplete = path_parameters_incomplete || !context.diagnostics.is_empty();
    endpoint
}

// Resuelve el cuerpo y su esquema con una pila de referencias aislada de otros cuerpos.
fn extract_body<'a>(
    context: &mut Resolution<'a>,
    body: &'a Value,
    visited: &VisitedReferences,
    uses_nullable: bool,
) -> Option<(FieldSchema, BTreeMap<String, FieldSchema>)> {
    let mut visited = visited.clone();
    let body = context.follow(body, &mut visited)?;
    let schema = context.follow(extract_json_schema(body)?, &mut visited)?;
    let root = body_schema(schema, context.sources(), uses_nullable);
    let mut fields = BTreeMap::new();
    collect_schema_fields(context, schema, "", &visited, &mut fields, uses_nullable);
    Some((root, fields))
}

/// Compara lo que el request acepta y lo que el response garantiza entre base y head.
///
/// Additive backwards-compatible changes (new endpoints, new response fields, new optional
/// request fields or optional parameters) are intentionally not reported as breaking changes.
pub fn diff_contracts(base: &ApiContract, head: &ApiContract) -> Vec<ContractChange> {
    let mut changes = Vec::new();

    for (endpoint_id, base_endpoint) in &base.endpoints {
        let Some(head_endpoint) = find_matching_endpoint(head, endpoint_id) else {
            if head
                .incomplete_paths
                .contains(&canonical_route_key(&endpoint_id.normalized_path))
            {
                continue;
            }
            changes.push(ContractChange::EndpointRemoved {
                endpoint: endpoint_id.clone(),
            });
            continue;
        };

        if base_endpoint.incomplete || head_endpoint.incomplete {
            continue;
        }

        for (status_code, base_body) in &base_endpoint.response_bodies {
            if let Some(head_body) = head_endpoint.response_bodies.get(status_code)
                && excludes_types(&head_body.field_type, &base_body.field_type)
            {
                changes.push(ContractChange::BodyTypeChanged {
                    endpoint: endpoint_id.clone(),
                    status_code: Some(status_code.clone()),
                    previous_type: base_body.field_type.clone(),
                    new_type: head_body.field_type.clone(),
                });
            }
        }
        if let (Some(base_body), Some(head_body)) =
            (&base_endpoint.request_body, &head_endpoint.request_body)
            && excludes_types(&base_body.field_type, &head_body.field_type)
        {
            changes.push(ContractChange::BodyTypeChanged {
                endpoint: endpoint_id.clone(),
                status_code: None,
                previous_type: base_body.field_type.clone(),
                new_type: head_body.field_type.clone(),
            });
        }

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
                    Some(head_field) => {
                        if excludes_types(&head_field.field_type, &base_field.field_type) {
                            changes.push(ContractChange::ResponseFieldTypeChanged {
                                endpoint: endpoint_id.clone(),
                                status_code: status_code.clone(),
                                field_path: field_path.clone(),
                                previous_type: base_field.field_type.clone(),
                                new_type: head_field.field_type.clone(),
                            });
                        }
                        if base_field.required && !head_field.required {
                            changes.push(ContractChange::ResponseFieldMadeOptional {
                                endpoint: endpoint_id.clone(),
                                status_code: status_code.clone(),
                                field_path: field_path.clone(),
                            });
                        }
                    }
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
                    if excludes_types(&base_field.field_type, &head_field.field_type) {
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
            let base_param = base_endpoint
                .parameters
                .get(&(location.clone(), name.clone()));
            if let Some(base_param) = base_param
                && excludes_types(&base_param.param_type, &head_param.param_type)
            {
                changes.push(ContractChange::RequestParameterTypeChanged {
                    endpoint: endpoint_id.clone(),
                    location: location.clone(),
                    name: name.clone(),
                    previous_type: base_param.param_type.clone(),
                    new_type: head_param.param_type.clone(),
                });
            }
            if head_param.required && !base_param.is_some_and(|parameter| parameter.required) {
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

// Extrae los parámetros resueltos y conserva el origen de la declaración y del tipo.
fn collect_parameters<'a>(
    context: &mut Resolution<'a>,
    params: &'a [Value],
    visited: &VisitedReferences,
    out: &mut BTreeMap<(String, String), ParameterSchema>,
    uses_nullable: bool,
) {
    for parameter in params {
        let mut visited = visited.clone();
        let Some(resolved) = context.follow(parameter, &mut visited) else {
            continue;
        };
        let Some(location) = resolved.get("in").and_then(Value::as_str) else {
            continue;
        };
        let Some(name_value) = resolved.get("name") else {
            continue;
        };
        let Some(name) = name_value.as_str() else {
            continue;
        };
        let required = location == "path"
            || resolved
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let schema = resolved
            .get("schema")
            .or_else(|| extract_json_schema(resolved))
            .and_then(|schema| context.follow(schema, &mut visited));
        out.insert(
            (location.to_owned(), name.to_owned()),
            ParameterSchema {
                param_type: schema.map_or_else(
                    || "unknown".to_owned(),
                    |schema| extract_type_label(schema, uses_nullable),
                ),
                required,
                type_source: schema.and_then(|node| context.source(node)).cloned(),
                source: context.source(name_value).cloned(),
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

// Aplana las propiedades y conserva la diferencia entre intersecciones y alternativas.
fn collect_schema_fields<'a>(
    context: &mut Resolution<'a>,
    schema: &'a Value,
    prefix: &str,
    visited: &VisitedReferences,
    out: &mut BTreeMap<String, FieldSchema>,
    uses_nullable: bool,
) {
    let mut visited = visited.clone();
    let Some(resolved) = context.follow(schema, &mut visited) else {
        return;
    };

    if let Some(all_of) = resolved.get("allOf").and_then(Value::as_array) {
        for child in all_of {
            collect_schema_fields(context, child, prefix, &visited, out, uses_nullable);
        }
        for child in all_of {
            if let Some(child) = context.follow(child, &mut visited.clone()) {
                apply_required_flags(child, prefix, out, context.sources());
            }
        }
    }

    for key in ["oneOf", "anyOf"] {
        if let Some(children) = resolved.get(key).and_then(Value::as_array) {
            let variants = children
                .iter()
                .filter(|child| child.as_bool() != Some(false))
                .map(|child| {
                    let mut fields = BTreeMap::new();
                    collect_schema_fields(
                        context,
                        child,
                        prefix,
                        &visited,
                        &mut fields,
                        uses_nullable,
                    );
                    fields
                })
                .collect::<Vec<_>>();
            merge_variants(out, variants);
        }
    }

    if let Some(props) = resolved.get("properties").and_then(Value::as_object) {
        for (name, schema) in props {
            let mut visited = visited.clone();
            let Some(property) = context.follow(schema, &mut visited) else {
                continue;
            };
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            let field_type = extract_type_label(property, uses_nullable);
            let source = context.source(schema).cloned();
            let type_source = (!std::ptr::eq(schema, property))
                .then(|| context.source(property))
                .flatten()
                .cloned();
            out.entry(path.clone())
                .and_modify(|field| {
                    if field_type != "unknown" {
                        field.field_type.clone_from(&field_type);
                        field.source.clone_from(&source);
                        field.type_source.clone_from(&type_source);
                    }
                    field.conditional = false;
                })
                .or_insert(FieldSchema {
                    field_type,
                    required: false,
                    conditional: false,
                    source,
                    type_source,
                    required_source: None,
                });
            collect_schema_fields(context, property, &path, &visited, out, uses_nullable);
        }
    }
    if let Some(items) = resolved
        .get("items")
        .filter(|items| items.is_object() || items.is_boolean())
    {
        collect_schema_fields(
            context,
            items,
            &format!("{prefix}[]"),
            &visited,
            out,
            uses_nullable,
        );
    }

    apply_required_flags(resolved, prefix, out, context.sources());
}

// Une los tipos de las variantes y conserva como requeridos solo los campos de todas ellas.
fn merge_variants(
    out: &mut BTreeMap<String, FieldSchema>,
    variants: Vec<BTreeMap<String, FieldSchema>>,
) {
    let total = variants.len();
    let mut fields: BTreeMap<String, (usize, FieldSchema)> = BTreeMap::new();
    for variant in variants {
        for (path, field) in variant {
            fields
                .entry(path)
                .and_modify(|(count, previous)| {
                    *count += 1;
                    previous.required &= field.required;
                    previous.conditional |= field.conditional;
                    previous.field_type = union_types(&previous.field_type, &field.field_type);
                })
                .or_insert((1, field));
        }
    }
    for (path, (count, mut field)) in fields {
        field.conditional |= count < total;
        field.required &= count == total;
        if !field.required {
            field.required_source = None;
        }
        out.entry(path)
            .and_modify(|existing| {
                existing.conditional &= field.conditional;
                if field.required && !existing.required {
                    existing.required = true;
                    existing.required_source.clone_from(&field.required_source);
                }
            })
            .or_insert(field);
    }
}

// Une tipos de ramas sin afirmar un conjunto exacto cuando una rama es desconocida.
fn union_types(a: &str, b: &str) -> String {
    if a == "unknown" || b == "unknown" {
        return "unknown".to_owned();
    }
    let mut types = a
        .split(" | ")
        .chain(b.split(" | "))
        .collect::<BTreeSet<_>>();
    if types.contains("number") {
        types.remove("integer");
    }
    types.into_iter().collect::<Vec<_>>().join(" | ")
}

// Vincula cada restricción required con el nombre exacto en el documento.
fn apply_required_flags(
    schema: &Value,
    prefix: &str,
    out: &mut BTreeMap<String, FieldSchema>,
    sources: &SourceIndex,
) {
    if let Some(arr) = schema.get("required").and_then(Value::as_array) {
        for item in arr {
            let Some(req_name) = item.as_str() else {
                continue;
            };
            let full_path = if prefix.is_empty() {
                req_name.to_string()
            } else {
                format!("{prefix}.{req_name}")
            };
            if let Some(field) = out.get_mut(&full_path) {
                field.required = true;
                field.required_source = sources.get(&std::ptr::from_ref(item)).cloned();
            }
        }
    }
}

// Conserva el tipo del cuerpo y el puntero al esquema raíz, incluso sin propiedades.
fn body_schema(schema: &Value, sources: &SourceIndex, uses_nullable: bool) -> FieldSchema {
    FieldSchema {
        field_type: extract_type_label(schema, uses_nullable),
        required: false,
        conditional: false,
        source: sources.get(&std::ptr::from_ref(schema)).cloned(),
        type_source: None,
        required_source: None,
    }
}

// Normaliza los tipos de la unión para que su orden y sus duplicados no afecten el diff.
fn extract_type_label(schema: &Value, uses_nullable: bool) -> String {
    let mut types = match schema.get("type") {
        Some(Value::String(ty)) => BTreeSet::from([ty.as_str()]),
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
        _ if schema.get("properties").is_some() => BTreeSet::from(["object"]),
        _ if schema.get("items").is_some() => BTreeSet::from(["array"]),
        _ => BTreeSet::new(),
    };
    if uses_nullable
        && schema.get("type").is_some_and(Value::is_string)
        && schema.get("nullable").and_then(Value::as_bool) == Some(true)
    {
        types.insert("null");
    }
    if types.contains("number") {
        types.remove("integer");
    }
    if types.is_empty() {
        "unknown".to_owned()
    } else {
        types.into_iter().collect::<Vec<_>>().join(" | ")
    }
}

// Busca un tipo del origen que el destino no admite. Todo entero también es número.
fn excludes_types(origin: &str, destination: &str) -> bool {
    origin != "unknown"
        && destination != "unknown"
        && origin.split(" | ").any(|part| {
            !destination
                .split(" | ")
                .any(|accepted| accepted == part || (accepted == "number" && part == "integer"))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::fixtures::create_demo_workspace;
    use crate::git::GitRepo;

    // Verifica que el BOM no altere el contrato ni sus líneas.
    #[test]
    fn accepts_utf8_bom_for_openapi() {
        let yaml = "openapi: 3.1.0\npaths:\n  /health:\n    get: {}\n";
        let parsed = extract_contract("api", yaml).unwrap();
        let marked = extract_contract("api", &format!("\u{feff}{yaml}")).unwrap();
        assert_eq!(parsed, marked);
    }

    // Comprueba equivalencia 3.0/3.1, compatibilidad direccional y cuerpos sin propiedades.
    #[test]
    fn compares_openapi_types_and_required_fields_directionally() {
        let base = r#"
openapi: 3.0.3
paths:
  /items:
    head: {}
    options: {}
    trace: {}
    post:
      parameters:
        - {in: query, name: tenant, required: true, schema: {type: integer}}
      requestBody:
        content:
          application/json:
            schema: {type: number}
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                required: [always]
                properties:
                  always: {type: string}
                  equivalent: {type: integer, nullable: true}
                  narrower: {type: number}
                  wider: {type: string}
        '201':
          content:
            application/json:
              schema: {type: string}
"#;
        let head = r#"
openapi: 3.1.0
paths:
  /items:
    post:
      parameters:
        - {in: query, name: tenant, required: true, schema: {type: boolean}}
      requestBody:
        content:
          application/json:
            schema: {type: integer}
      responses:
        '200':
          content:
            application/json:
              schema:
                type: object
                properties:
                  always: {type: string}
                  equivalent: {type: ['null', integer, integer]}
                  narrower: {type: integer}
                  wider: {type: [string, 'null']}
        '201':
          content:
            application/json:
              schema: {type: [string, integer]}
"#;
        let before = extract_contract("api", base).unwrap();
        let after = extract_contract("api", head).unwrap();
        let changes = diff_contracts(&before, &after);
        assert_eq!(changes.len(), 8, "{changes:?}");
        assert!(changes.iter().any(|change| matches!(change, ContractChange::ResponseFieldMadeOptional { field_path, .. } if field_path == "always")));
        assert!(changes.iter().any(|change| matches!(change, ContractChange::ResponseFieldTypeChanged { field_path, .. } if field_path == "wider")));
        let param = changes.iter().find(|change| matches!(change, ContractChange::RequestParameterTypeChanged { name, .. } if name == "tenant")).unwrap();
        assert!(
            before
                .source_for_change(param)
                .unwrap()
                .pointer
                .ends_with("/0/schema")
        );
        for status in [None, Some("201")] {
            let body = changes.iter().find(|change| matches!(change, ContractChange::BodyTypeChanged { status_code, .. } if status_code.as_deref() == status)).unwrap();
            assert!(
                after
                    .source_for_change(body)
                    .unwrap()
                    .pointer
                    .ends_with("/schema")
            );
        }
        for method in [HttpMethod::Head, HttpMethod::Options, HttpMethod::Trace] {
            assert!(changes.iter().any(|change| matches!(change, ContractChange::EndpointRemoved { endpoint } if endpoint.method == method)));
        }

        let widened_request = head.replace("schema: {type: integer}", "schema: {type: number}");
        assert!(
            !diff_contracts(&after, &extract_contract("api", &widened_request).unwrap())
                .iter()
                .any(|change| matches!(
                    change,
                    ContractChange::BodyTypeChanged {
                        status_code: None,
                        ..
                    }
                ))
        );
    }

    // Comprueba que reordenar una unión de tipos JSON Schema no altera el contrato.
    #[test]
    fn ignores_openapi_type_union_order() {
        let spec = r#"
openapi: 3.1.0
paths:
  /items:
    get:
      responses:
        '200':
          content:
            application/json:
              schema: {type: [integer, 'null']}
"#;
        let before = extract_contract("api", spec).unwrap();
        let after =
            extract_contract("api", &spec.replace("integer, 'null'", "'null', integer")).unwrap();
        assert!(diff_contracts(&before, &after).is_empty());
    }

    // Verifica la unión de campos de variantes, sus garantías y el archivo de origen.
    #[test]
    fn resolves_external_openapi_references_and_marks_conditional_fields() {
        let main = r#"
openapi: 3.1.0
paths:
  /items:
    get:
      responses:
        '200':
          content:
            application/json:
              schema: {$ref: '../models/item%20model.yaml#/definitions/Item'}
"#;
        let model = r#"
definitions:
  Item:
    oneOf:
      - $ref: '#/definitions/A'
      - $ref: 'other.json#/B'
  A:
    type: object
    required: [shared, exclusive]
    properties:
      shared: {type: string}
      exclusive: {type: boolean}
"#;
        let make = |other: &str| {
            extract_contract_with_references("api", "specs/api.yaml", main, |file| match file {
                "models/item model.yaml" => Some(model.to_owned()),
                "models/other.json" => Some(other.to_owned()),
                _ => panic!("unexpected file request: {file}"),
            })
            .unwrap()
        };
        let before = make(
            r#"{"B":{"anyOf":[{"type":"object","required":["shared"],"properties":{"shared":{"type":"integer"}}},false]}}"#,
        );
        assert!(before.diagnostics.is_empty());
        assert_eq!(before.loaded_files, 3);
        let endpoint = before.contract.endpoints.values().next().unwrap();
        let fields = &endpoint.responses["200"];
        assert!(fields["shared"].required);
        assert!(!fields["shared"].conditional);
        assert_eq!(fields["shared"].field_type, "integer | string");
        assert!(!fields["exclusive"].required);
        assert!(fields["exclusive"].conditional);
        assert_eq!(
            fields["exclusive"].source.as_ref().unwrap().file,
            "models/item model.yaml"
        );

        let after = make(r#"{"B":{"type":"object","properties":{"shared":{"type":"integer"}}}}"#);
        let diff = diff_contracts(&before.contract, &after.contract);
        assert!(
            matches!(diff.as_slice(), [ContractChange::ResponseFieldMadeOptional { field_path, .. }] if field_path == "shared")
        );
    }

    // Comprueba que los ciclos y los destinos inválidos mantienen parciales solo las rutas afectadas.
    #[test]
    fn reports_cycles_and_confines_openapi_reference_paths() {
        let root = r#"
paths:
  /gone: {$ref: './missing.yaml'}
  /loop:
    get:
      responses:
        '200':
          content:
            application/json:
              schema: {$ref: 'schemas/loop.yaml#/Cycle'}
  /outside: {$ref: '%2e%2e/%2e%2e/secret.yaml'}
  /remote: {$ref: 'https://example.test/remote.yaml'}
"#;
        let extraction = extract_contract_with_references("api", "docs/spec.yaml", root, |file| match file {
            "docs/missing.yaml" => None,
            "docs/schemas/loop.yaml" => Some("Cycle: {$ref: '../spec.yaml#/paths/~1loop/get/responses/200/content/application~1json/schema'}".into()),
            _ => panic!("unconfined file read: {file}"),
        }).unwrap();
        assert_eq!(extraction.diagnostics.len(), 4);
        assert_eq!(
            extraction
                .diagnostics
                .iter()
                .filter(|(_, code)| *code == ExtractionFailure::OpenApiReferenceCycle)
                .count(),
            1
        );
        let base = extract_contract(
            "api",
            "paths:\n  /gone: {get: {}}\n  /unrelated: {get: {}}\n",
        )
        .unwrap();
        assert!(matches!(
            diff_contracts(&base, &extraction.contract).as_slice(),
            [ContractChange::EndpointRemoved {endpoint}] if endpoint.normalized_path == "/unrelated"
        ));
    }

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

        let source = base_contract.source_for_change(&changes[0]).unwrap();
        assert!(source.span.as_ref().unwrap().line > 1);

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

        let source = head.source_for_change(&changes[2]).unwrap();
        assert!(source.span.as_ref().unwrap().line > 1);
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
