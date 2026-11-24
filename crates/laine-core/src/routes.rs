use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{ContractChange, ContractDiscrepancy, DiscrepancyKind, EndpointId, HttpMethod};
use crate::openapi::{ApiContract, canonical_route_key};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedRoute {
    pub endpoint: EndpointId,
    pub file: String,
    pub line: usize,
    pub snippet: String,
}

/// Joins optional prefixes and normalizes Laravel `{param}` / `{param?}` or Express `:param`
/// segments into Laine's canonical display path (e.g. `/corp/bookings/:id`).
pub fn normalize_route_path(prefixes: &[&str], raw_path: &str) -> String {
    let mut segments = Vec::new();

    for part in prefixes.iter().copied().chain(std::iter::once(raw_path)) {
        for raw_seg in part.trim().split('/') {
            let seg = raw_seg.trim();
            if seg.is_empty() {
                continue;
            }
            if seg.starts_with('{') && seg.ends_with('}') && seg.len() > 2 {
                let inner = seg[1..seg.len() - 1].trim_end_matches('?');
                segments.push(format!(":{inner}"));
            } else {
                segments.push(seg.to_string());
            }
        }
    }

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Compares producer routes extracted from code between `base` and `head`.
/// Returns `ContractChange::EndpointRemoved` for every route present in `base` and missing in `head`.
pub fn diff_routes(base: &[ExtractedRoute], head: &[ExtractedRoute]) -> Vec<ContractChange> {
    let base_map = index_routes(base);
    let head_map = index_routes(head);

    let mut changes = Vec::new();
    for (key, route) in base_map {
        if !head_map.contains_key(&key) {
            changes.push(ContractChange::EndpointRemoved {
                endpoint: route.endpoint,
            });
        }
    }
    changes
}

/// Detects mismatches between an OpenAPI specification and routes extracted from backend code.
pub fn detect_openapi_discrepancies(
    openapi: &ApiContract,
    code_routes: &[ExtractedRoute],
) -> Vec<ContractDiscrepancy> {
    let mut openapi_keys: BTreeMap<(String, HttpMethod, String), EndpointId> = BTreeMap::new();
    for endpoint in openapi.endpoints.keys() {
        let key = (
            endpoint.service.clone(),
            endpoint.method,
            canonical_route_key(&endpoint.normalized_path),
        );
        openapi_keys.entry(key).or_insert_with(|| endpoint.clone());
    }

    let code_keys = index_routes(code_routes);
    let mut discrepancies = Vec::new();

    for (key, route) in &code_keys {
        if !openapi_keys.contains_key(key) {
            discrepancies.push(ContractDiscrepancy {
                endpoint: route.endpoint.clone(),
                kind: DiscrepancyKind::UndocumentedInOpenApi,
                detail: format!(
                    "Route exposed in {}:{} is missing from the OpenAPI contract",
                    route.file, route.line
                ),
            });
        }
    }

    for (key, endpoint) in &openapi_keys {
        if !code_keys.contains_key(key) {
            discrepancies.push(ContractDiscrepancy {
                endpoint: endpoint.clone(),
                kind: DiscrepancyKind::MissingInCode,
                detail: "Endpoint declared in OpenAPI was not found in extracted route code"
                    .to_string(),
            });
        }
    }

    discrepancies
}

fn index_routes(
    routes: &[ExtractedRoute],
) -> BTreeMap<(String, HttpMethod, String), ExtractedRoute> {
    let mut map = BTreeMap::new();
    for route in routes {
        let key = (
            route.endpoint.service.clone(),
            route.endpoint.method,
            canonical_route_key(&route.endpoint.normalized_path),
        );
        map.entry(key).or_insert_with(|| route.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::extract_contract;

    fn mk_route(method: HttpMethod, path: &str) -> ExtractedRoute {
        ExtractedRoute {
            endpoint: EndpointId {
                service: "customer-service".into(),
                method,
                normalized_path: path.into(),
            },
            file: "routes/api.php".into(),
            line: 10,
            snippet: format!("Route::{}('{path}')", method.as_str().to_lowercase()),
        }
    }

    #[test]
    fn diffs_removed_routes_ignoring_parameter_renames() {
        let base = vec![
            mk_route(HttpMethod::Get, "/customers/:id"),
            mk_route(HttpMethod::Post, "/corp/listBooking"),
        ];
        let head = vec![
            mk_route(HttpMethod::Get, "/customers/:customerId"),
            mk_route(HttpMethod::Post, "/corp/abortLocalBooking"),
        ];

        let changes = diff_routes(&base, &head);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0],
            ContractChange::EndpointRemoved {
                endpoint: EndpointId {
                    service: "customer-service".into(),
                    method: HttpMethod::Post,
                    normalized_path: "/corp/listBooking".into(),
                }
            }
        );
    }

    #[test]
    fn detects_discrepancies_between_openapi_and_code_routes() {
        let spec = r#"
openapi: 3.0.3
paths:
  /customers/{id}:
    get:
      responses:
        '200':
          description: ok
  /legacy/ghost:
    get:
      responses:
        '200':
          description: ok
"#;
        let openapi = extract_contract("customer-service", spec).unwrap();
        let code_routes = vec![
            mk_route(HttpMethod::Get, "/customers/:id"),
            mk_route(HttpMethod::Post, "/internal/secret"),
        ];

        let discrepancies = detect_openapi_discrepancies(&openapi, &code_routes);
        assert_eq!(discrepancies.len(), 2);
        assert!(discrepancies.iter().any(|d| {
            d.kind == DiscrepancyKind::UndocumentedInOpenApi
                && d.endpoint.normalized_path == "/internal/secret"
        }));
        assert!(discrepancies.iter().any(|d| {
            d.kind == DiscrepancyKind::MissingInCode
                && d.endpoint.normalized_path == "/legacy/ghost"
        }));
    }
}
