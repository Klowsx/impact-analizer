use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::model::{Confidence, HttpMethod};
use crate::openapi::canonical_route_key;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GraphNode {
    Repository {
        name: String,
    },
    File {
        repo: String,
        path: String,
    },
    Endpoint {
        service: String,
        method: HttpMethod,
        canonical_path: String,
        display_path: String,
    },
    SchemaField {
        service: String,
        method: HttpMethod,
        canonical_path: String,
        field_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphEdge {
    Contains,
    Exposes,
    ReturnsField,
    Consumes {
        revision: String,
        line: usize,
        snippet: String,
        confidence: Confidence,
    },
    ReadsField {
        revision: String,
        line: usize,
        snippet: String,
        call_file: String,
        call_line: usize,
        confidence: Confidence,
        assignment_confidence: Confidence,
        custom_trace: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldConsumerHit {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
    pub call_file: String,
    pub call_line: usize,
    pub matched_field: String,
    pub confidence: Confidence,
    pub source_confidence: Confidence,
    pub custom_trace: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConsumerHit {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
    pub confidence: Confidence,
}

/// Directed dependency graph built on `petgraph::DiGraph` with automatic node deduplication.
#[derive(Debug, Default)]
pub struct DependencyGraph {
    graph: DiGraph<GraphNode, GraphEdge>,
    repos: HashMap<String, NodeIndex>,
    files: HashMap<(String, String), NodeIndex>,
    endpoints: HashMap<(String, HttpMethod, String), NodeIndex>,
    catalog_endpoints: HashMap<(String, HttpMethod), BTreeMap<String, NodeIndex>>,
    fields: HashMap<(String, HttpMethod, String, String), NodeIndex>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn add_repository(&mut self, name: &str) -> NodeIndex {
        if let Some(&idx) = self.repos.get(name) {
            return idx;
        }
        let idx = self.graph.add_node(GraphNode::Repository {
            name: name.to_string(),
        });
        self.repos.insert(name.to_string(), idx);
        idx
    }

    pub fn add_file(&mut self, repo: &str, path: &str) -> NodeIndex {
        let key = (repo.to_string(), path.to_string());
        if let Some(&idx) = self.files.get(&key) {
            return idx;
        }
        let repo_idx = self.add_repository(repo);
        let file_idx = self.graph.add_node(GraphNode::File {
            repo: repo.to_string(),
            path: path.to_string(),
        });
        self.files.insert(key, file_idx);
        self.graph.add_edge(repo_idx, file_idx, GraphEdge::Contains);
        file_idx
    }

    // Registra una operación del productor como candidata para resolver llamadas.
    pub fn add_endpoint(
        &mut self,
        service: &str,
        method: HttpMethod,
        display_path: &str,
    ) -> NodeIndex {
        let canonical_path = canonical_route_key(display_path);
        let endpoint = self.get_or_create_endpoint(service, method, display_path, &canonical_path);
        self.catalog_endpoints
            .entry((service.to_string(), method))
            .or_default()
            .insert(canonical_path, endpoint);
        endpoint
    }

    // Reutiliza la identidad canónica o crea un nodo para el grafo de dependencias.
    fn get_or_create_endpoint(
        &mut self,
        service: &str,
        method: HttpMethod,
        display_path: &str,
        canonical_path: &str,
    ) -> NodeIndex {
        let key = (service.to_string(), method, canonical_path.to_string());
        if let Some(&idx) = self.endpoints.get(&key) {
            return idx;
        }
        let repo_idx = self.add_repository(service);
        let ep_idx = self.graph.add_node(GraphNode::Endpoint {
            service: service.to_string(),
            method,
            canonical_path: canonical_path.to_string(),
            display_path: display_path.to_string(),
        });
        self.endpoints.insert(key, ep_idx);
        self.graph.add_edge(repo_idx, ep_idx, GraphEdge::Exposes);
        ep_idx
    }

    // Registra un campo declarado en el contrato de la operación.
    pub fn add_schema_field(
        &mut self,
        service: &str,
        method: HttpMethod,
        display_path: &str,
        field_path: &str,
    ) -> NodeIndex {
        let endpoint = self.add_endpoint(service, method, display_path);
        self.add_field_to_endpoint(endpoint, field_path)
    }

    // Asocia un campo a una operación ya resuelta sin añadir una ruta al catálogo.
    fn add_field_to_endpoint(&mut self, endpoint: NodeIndex, field_path: &str) -> NodeIndex {
        let GraphNode::Endpoint {
            service,
            method,
            canonical_path,
            ..
        } = &self.graph[endpoint]
        else {
            unreachable!("fields must belong to an endpoint");
        };
        let key = (
            service.clone(),
            *method,
            canonical_path.clone(),
            field_path.to_string(),
        );
        if let Some(&idx) = self.fields.get(&key) {
            return idx;
        }
        let field_idx = self.graph.add_node(GraphNode::SchemaField {
            service: key.0.clone(),
            method: key.1,
            canonical_path: key.2.clone(),
            field_path: key.3.clone(),
        });
        self.fields.insert(key, field_idx);
        self.graph
            .add_edge(endpoint, field_idx, GraphEdge::ReturnsField);
        field_idx
    }

    #[allow(clippy::too_many_arguments)]
    // Vincula la llamada y devuelve el primer nodo en orden canónico cuando hay varios candidatos.
    pub fn link_consumes(
        &mut self,
        consumer_repo: &str,
        consumer_file: &str,
        revision: &str,
        target_service: &str,
        method: HttpMethod,
        consumer_path: &str,
        line: usize,
        snippet: &str,
    ) -> NodeIndex {
        let file_idx = self.add_file(consumer_repo, consumer_file);
        let assignments = self.assign_endpoints(target_service, method, consumer_path);

        for &(endpoint, confidence) in &assignments {
            self.graph.add_edge(
                file_idx,
                endpoint,
                GraphEdge::Consumes {
                    revision: revision.to_string(),
                    line,
                    snippet: snippet.to_string(),
                    confidence,
                },
            );
        }
        assignments[0].0
    }

    #[allow(clippy::too_many_arguments)]
    pub fn link_reads_field(
        &mut self,
        consumer_repo: &str,
        consumer_file: &str,
        revision: &str,
        target_service: &str,
        method: HttpMethod,
        consumer_path: &str,
        field_path: &str,
        line: usize,
        snippet: &str,
        call_line: usize,
    ) {
        self.link_reads_field_with_confidence(
            consumer_repo,
            consumer_file,
            revision,
            target_service,
            method,
            consumer_path,
            field_path,
            line,
            snippet,
            consumer_file,
            call_line,
            Confidence::Confirmed,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn link_reads_field_with_confidence(
        &mut self,
        consumer_repo: &str,
        consumer_file: &str,
        revision: &str,
        target_service: &str,
        method: HttpMethod,
        consumer_path: &str,
        field_path: &str,
        line: usize,
        snippet: &str,
        call_file: &str,
        call_line: usize,
        confidence: Confidence,
    ) {
        self.link_reads_field_with_trace(
            consumer_repo,
            consumer_file,
            revision,
            target_service,
            method,
            consumer_path,
            field_path,
            line,
            snippet,
            call_file,
            call_line,
            confidence,
            &[],
        );
    }

    #[allow(clippy::too_many_arguments)]
    // Asocia una lectura a las mismas operaciones candidatas que su llamada.
    pub fn link_reads_field_with_trace(
        &mut self,
        consumer_repo: &str,
        consumer_file: &str,
        revision: &str,
        target_service: &str,
        method: HttpMethod,
        consumer_path: &str,
        field_path: &str,
        line: usize,
        snippet: &str,
        call_file: &str,
        call_line: usize,
        confidence: Confidence,
        custom_trace: &[String],
    ) {
        let file_idx = self.add_file(consumer_repo, consumer_file);

        for (endpoint, assignment_confidence) in
            self.assign_endpoints(target_service, method, consumer_path)
        {
            let field_idx = self.add_field_to_endpoint(endpoint, field_path);
            self.graph.add_edge(
                file_idx,
                field_idx,
                GraphEdge::ReadsField {
                    revision: revision.to_string(),
                    line,
                    snippet: snippet.to_string(),
                    call_file: call_file.to_string(),
                    call_line,
                    confidence,
                    assignment_confidence,
                    custom_trace: custom_trace.to_vec(),
                },
            );
        }
    }

    /// Busca lecturas del campo y sus descendientes dentro de la misma operación.
    pub fn consumers_of_field(
        &self,
        service: &str,
        method: HttpMethod,
        producer_path: &str,
        changed_field: &str,
    ) -> Vec<FieldConsumerHit> {
        let key = (
            service.to_string(),
            method,
            canonical_route_key(producer_path),
        );
        let Some(&endpoint) = self.endpoints.get(&key) else {
            return Vec::new();
        };
        let mut dedup: BTreeMap<(String, String, usize, String), FieldConsumerHit> =
            BTreeMap::new();
        let strength = |hit: &FieldConsumerHit| {
            (
                hit.confidence,
                hit.source_confidence,
                !hit.custom_trace.is_empty(),
            )
        };

        for field_edge in self.graph.edges_directed(endpoint, Direction::Outgoing) {
            let field_idx = field_edge.target();
            let GraphNode::SchemaField { field_path, .. } = &self.graph[field_idx] else {
                continue;
            };
            if !field_matches(changed_field, field_path) {
                continue;
            }

            for edge in self.graph.edges_directed(field_idx, Direction::Incoming) {
                if let GraphEdge::ReadsField {
                    revision,
                    line,
                    snippet,
                    call_file,
                    call_line,
                    confidence,
                    assignment_confidence,
                    custom_trace,
                } = edge.weight()
                    && let GraphNode::File { repo, path } = &self.graph[edge.source()]
                {
                    let key = (repo.clone(), path.clone(), *line, field_path.clone());
                    let candidate = FieldConsumerHit {
                        repo: repo.clone(),
                        revision: revision.clone(),
                        file: path.clone(),
                        line: *line,
                        snippet: snippet.clone(),
                        call_file: call_file.clone(),
                        call_line: *call_line,
                        matched_field: field_path.clone(),
                        confidence: (*confidence).min(*assignment_confidence),
                        source_confidence: *confidence,
                        custom_trace: custom_trace.clone(),
                    };
                    if dedup
                        .get(&key)
                        .is_none_or(|hit| strength(&candidate) > strength(hit))
                    {
                        dedup.insert(key, candidate);
                    }
                }
            }
        }

        dedup.into_values().collect()
    }

    /// Busca las llamadas vinculadas a la identidad exacta de la operación.
    pub fn consumers_of_endpoint(
        &self,
        service: &str,
        method: HttpMethod,
        producer_path: &str,
    ) -> Vec<EndpointConsumerHit> {
        let key = (
            service.to_string(),
            method,
            canonical_route_key(producer_path),
        );
        let Some(&endpoint) = self.endpoints.get(&key) else {
            return Vec::new();
        };
        let mut hits = BTreeMap::<_, EndpointConsumerHit>::new();

        for edge in self.graph.edges_directed(endpoint, Direction::Incoming) {
            if let GraphEdge::Consumes {
                revision,
                line,
                snippet,
                confidence,
            } = edge.weight()
                && let GraphNode::File { repo, path } = &self.graph[edge.source()]
            {
                let key = (repo.clone(), path.clone(), *line);
                if hits.get(&key).is_none_or(|hit| {
                    (*confidence, revision, snippet) > (hit.confidence, &hit.revision, &hit.snippet)
                }) {
                    hits.insert(
                        key,
                        EndpointConsumerHit {
                            repo: repo.clone(),
                            revision: revision.clone(),
                            file: path.clone(),
                            line: *line,
                            snippet: snippet.clone(),
                            confidence: *confidence,
                        },
                    );
                }
            }
        }

        hits.into_values().collect()
    }

    /// Performs a cycle-safe BFS to discover all repositories transitively depending on `target_repo`.
    pub fn transitive_dependent_repos(&self, target_repo: &str) -> Vec<String> {
        self.transitive_dependent_paths(target_repo)
            .into_iter()
            .map(|(repo, _)| repo)
            .collect()
    }

    /// Performs a cycle-safe BFS from `start_repo`, returning `(downstream_repo, path_from_start)`
    /// sorted deterministically by repository name.
    pub fn transitive_dependent_paths(&self, start_repo: &str) -> Vec<(String, Vec<String>)> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut results: Vec<(String, Vec<String>)> = Vec::new();

        visited.insert(start_repo.to_string());
        queue.push_back((start_repo.to_string(), vec![start_repo.to_string()]));

        while let Some((current_repo, path)) = queue.pop_front() {
            for direct in self.direct_dependent_repos(&current_repo) {
                if visited.insert(direct.clone()) {
                    let mut next_path = path.clone();
                    next_path.push(direct.clone());
                    results.push((direct.clone(), next_path.clone()));
                    queue.push_back((direct, next_path));
                }
            }
        }

        results.sort_by(|a, b| a.0.cmp(&b.0));
        results
    }

    fn direct_dependent_repos(&self, service: &str) -> BTreeSet<String> {
        let mut repos = BTreeSet::new();
        for ((svc, _, _), &ep_idx) in &self.endpoints {
            if svc != service {
                continue;
            }
            for edge in self.graph.edges_directed(ep_idx, Direction::Incoming) {
                if matches!(edge.weight(), GraphEdge::Consumes { .. })
                    && let GraphNode::File { repo, .. } = &self.graph[edge.source()]
                    && repo != service
                {
                    repos.insert(repo.clone());
                }
            }
        }
        repos
    }

    // Prioriza coincidencias literales y conserva las asignaciones ambiguas como posibles.
    fn assign_endpoints(
        &mut self,
        service: &str,
        method: HttpMethod,
        consumer_path: &str,
    ) -> Vec<(NodeIndex, Confidence)> {
        let consumer_canonical = canonical_route_key(consumer_path);
        let dynamic =
            route_segments(&consumer_canonical).any(|segment| matches!(segment, "*" | "*?"));
        let catalog = self.catalog_endpoints.get(&(service.to_string(), method));
        if !dynamic && let Some(&exact) = catalog.and_then(|routes| routes.get(&consumer_canonical))
        {
            return vec![(exact, Confidence::Confirmed)];
        }

        let candidates: Vec<_> = catalog
            .into_iter()
            .flatten()
            .filter(|(path, _)| routes_match(path, &consumer_canonical))
            .collect();

        if candidates.is_empty() {
            // Conserva la dependencia entre repositorios sin atribuirla a una operación declarada.
            let endpoint =
                self.get_or_create_endpoint(service, method, consumer_path, &consumer_canonical);
            return vec![(endpoint, Confidence::Possible)];
        }

        let confidence = if candidates.len() == 1
            && (!dynamic || route_covers(candidates[0].0, &consumer_canonical, false))
        {
            Confidence::Confirmed
        } else {
            Confidence::Possible
        };
        candidates
            .into_iter()
            .map(|(_, &endpoint)| (endpoint, confidence))
            .collect()
    }
}

/// Checks whether two canonical route keys match segment-by-segment, treating `*` as a required
/// wildcard (1 segment) and `*?` as an optional wildcard (0 or 1 segment).
pub fn routes_match(producer_canonical: &str, consumer_canonical: &str) -> bool {
    let prod_segs: Vec<&str> = producer_canonical
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let cons_segs: Vec<&str> = consumer_canonical
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    segments_match(&prod_segs, &cons_segs)
}

// Separa una ruta canónica en segmentos para interpretar parámetros y literales.
fn route_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|segment| !segment.is_empty())
}

const MAX_ROUTE_COVERAGE_STATES: usize = 4096;

// Comprueba si el patrón acepta todas las rutas del candidato, incluidos sus segmentos opcionales.
pub(crate) fn route_covers(pattern: &str, candidate: &str, recursive_prefix: bool) -> bool {
    if pattern == candidate {
        return true;
    }

    let pattern: Vec<_> = route_segments(pattern).collect();
    let candidate: Vec<_> = route_segments(candidate).collect();
    if recursive_prefix
        && pattern.len() <= candidate.len()
        && pattern.iter().zip(&candidate).all(|(pattern, candidate)| {
            *candidate != "*?" && (*pattern == "*" || pattern == candidate)
        })
    {
        return true;
    }
    if !pattern.contains(&"*?") && !candidate.contains(&"*?") {
        return (recursive_prefix || pattern.len() == candidate.len())
            && pattern.len() <= candidate.len()
            && pattern
                .iter()
                .zip(&candidate)
                .all(|(pattern, candidate)| *pattern == "*" || pattern == candidate);
    }

    let mut pattern_start = BTreeSet::new();
    let mut candidate_start = BTreeSet::new();
    include_optional_positions(&pattern, &mut pattern_start, 0);
    include_optional_positions(&candidate, &mut candidate_start, 0);
    let start = (pattern_start, candidate_start);
    let mut visited = HashSet::from([start.clone()]);
    let mut pending = VecDeque::from([start]);

    while let Some((pattern_positions, candidate_positions)) = pending.pop_front() {
        if candidate_positions.contains(&candidate.len())
            && !pattern_positions.contains(&pattern.len())
        {
            return false;
        }
        // Solo se distinguen los literales que pueden consumirse desde el estado actual.
        let segments: BTreeSet<_> = pattern_positions
            .iter()
            .filter_map(|&pos| pattern.get(pos))
            .chain(
                candidate_positions
                    .iter()
                    .filter_map(|&pos| candidate.get(pos)),
            )
            .copied()
            .filter(|segment| !matches!(*segment, "*" | "*?"))
            .collect();
        for segment in segments.iter().copied().map(Some).chain([None]) {
            let next_candidate = advance_route(&candidate, &candidate_positions, segment, false);
            if next_candidate.is_empty() {
                continue;
            }
            let next_pattern =
                advance_route(&pattern, &pattern_positions, segment, recursive_prefix);
            let next = (next_pattern, next_candidate);
            if visited.insert(next.clone()) {
                // Si no se puede demostrar la cobertura, se conserva el hallazgo o la duda.
                if visited.len() > MAX_ROUTE_COVERAGE_STATES {
                    return false;
                }
                pending.push_back(next);
            }
        }
    }

    true
}

// Añade las posiciones alcanzables sin consumir un segmento al omitir parámetros opcionales.
fn include_optional_positions(route: &[&str], positions: &mut BTreeSet<usize>, mut pos: usize) {
    loop {
        positions.insert(pos);
        if route.get(pos) != Some(&"*?") {
            break;
        }
        pos += 1;
    }
}

// Consume un segmento conocido o uno genérico y conserva las continuaciones válidas.
fn advance_route(
    route: &[&str],
    positions: &BTreeSet<usize>,
    segment: Option<&str>,
    recursive_prefix: bool,
) -> BTreeSet<usize> {
    let mut next = BTreeSet::new();
    for &pos in positions {
        match route.get(pos).copied() {
            Some(expected) if matches!(expected, "*" | "*?") || Some(expected) == segment => {
                include_optional_positions(route, &mut next, pos + 1);
            }
            None if recursive_prefix => {
                next.insert(pos);
            }
            _ => {}
        }
    }
    next
}

fn segments_match(a: &[&str], b: &[&str]) -> bool {
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[m][n] = true;

    for i in (0..=m).rev() {
        for j in (0..=n).rev() {
            if i == m && j == n {
                continue;
            }
            let mut matched = false;
            if i < m && a[i] == "*?" {
                matched |= dp[i + 1][j];
                if j < n {
                    matched |= dp[i + 1][j + 1];
                }
            }
            if j < n && b[j] == "*?" {
                matched |= dp[i][j + 1];
                if i < m {
                    matched |= dp[i + 1][j + 1];
                }
            }
            if i < m
                && j < n
                && a[i] != "*?"
                && b[j] != "*?"
                && (a[i] == "*" || b[j] == "*" || a[i] == b[j])
            {
                matched |= dp[i + 1][j + 1];
            }
            dp[i][j] = matched;
        }
    }

    dp[0][0]
}

fn field_matches(changed_field: &str, read_field: &str) -> bool {
    if read_field == changed_field {
        return true;
    }
    // Reading `address.city` is affected if parent object `address` was removed/changed.
    read_field
        .strip_prefix(changed_field)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("[]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Registra una llamada y su lectura para comparar ambas consultas de impacto.
    fn link_call_and_read(graph: &mut DependencyGraph, path: &str, line: usize) {
        graph.link_consumes(
            "app",
            "client.ts",
            "rev",
            "api",
            HttpMethod::Get,
            path,
            line,
            "fetch(...)",
        );
        graph.link_reads_field(
            "app",
            "client.ts",
            "rev",
            "api",
            HttpMethod::Get,
            path,
            "user.name",
            line,
            "user.name",
            line,
        );
    }

    // Devuelve líneas y niveles de confianza de llamadas y lecturas para la misma operación.
    fn operation_hits(graph: &DependencyGraph, path: &str) -> [Vec<(usize, Confidence)>; 2] {
        let endpoint = graph.consumers_of_endpoint("api", HttpMethod::Get, path);
        let field = graph.consumers_of_field("api", HttpMethod::Get, path, "user");
        [
            endpoint
                .into_iter()
                .map(|hit| (hit.line, hit.confidence))
                .collect(),
            field
                .into_iter()
                .map(|hit| (hit.line, hit.confidence))
                .collect(),
        ]
    }

    // Separa la llamada literal y conserva una llamada dinámica como candidata de las dos rutas.
    #[test]
    fn preserves_literal_and_template_operation_identity() {
        let mut graph = DependencyGraph::new();
        graph.add_endpoint("api", HttpMethod::Get, "/users/me");
        graph.add_endpoint("api", HttpMethod::Get, "/users/{id}");

        link_call_and_read(&mut graph, "/users/me", 1);
        link_call_and_read(&mut graph, "/users/42", 2);
        link_call_and_read(&mut graph, "/users/:id", 3);

        for hits in operation_hits(&graph, "/users/me") {
            assert_eq!(
                hits,
                [(1, Confidence::Confirmed), (3, Confidence::Possible)]
            );
        }
        for hits in operation_hits(&graph, "/users/{another}") {
            assert_eq!(
                hits,
                [(2, Confidence::Confirmed), (3, Confidence::Possible)]
            );
        }
        assert!(
            graph
                .consumers_of_endpoint("api", HttpMethod::Post, "/users/{id}")
                .is_empty()
        );
        assert!(
            graph
                .consumers_of_field("other", HttpMethod::Get, "/users/me", "user")
                .is_empty()
        );
    }

    // Comprueba los parámetros omitidos y presentes y la prioridad de las rutas literales.
    #[test]
    fn matches_optional_parameters_unless_a_literal_has_priority() {
        let mut graph = DependencyGraph::new();
        graph.add_endpoint("api", HttpMethod::Get, "/users/{id?}");
        graph.add_endpoint("api", HttpMethod::Get, "/items/{id?}");
        graph.add_endpoint("api", HttpMethod::Get, "/items");
        link_call_and_read(&mut graph, "/users", 1);
        link_call_and_read(&mut graph, "/users/42", 2);
        link_call_and_read(&mut graph, "/items", 3);
        link_call_and_read(&mut graph, "/items/42", 4);

        for hits in operation_hits(&graph, "/users/{id?}") {
            assert_eq!(
                hits,
                [(1, Confidence::Confirmed), (2, Confidence::Confirmed)]
            );
        }
        for hits in operation_hits(&graph, "/items/{id?}") {
            assert_eq!(hits, [(4, Confidence::Confirmed)]);
        }
        for hits in operation_hits(&graph, "/items") {
            assert_eq!(hits, [(3, Confidence::Confirmed)]);
        }
    }

    // Conserva las plantillas superpuestas como candidatas sin importar su orden de registro.
    #[test]
    fn reports_overlapping_templates_with_possible_confidence() {
        let first = "/groups/current/:id";
        let second = "/groups/:slug/items";
        for operations in [[first, second], [second, first]] {
            let mut graph = DependencyGraph::new();
            for path in operations {
                graph.add_endpoint("api", HttpMethod::Get, path);
            }
            link_call_and_read(&mut graph, "/groups/current/items", 1);
            for path in operations {
                for hits in operation_hits(&graph, path) {
                    assert_eq!(hits, [(1, Confidence::Possible)]);
                }
            }
        }
    }

    // Mantiene las dependencias entre servicios sin usar rutas inferidas para resolver otras llamadas.
    #[test]
    fn synthetic_endpoints_do_not_capture_other_calls() {
        let mut graph = DependencyGraph::new();
        link_call_and_read(&mut graph, "/users/:id", 1);
        link_call_and_read(&mut graph, "/users/me", 2);

        for hits in operation_hits(&graph, "/users/:id") {
            assert_eq!(hits, [(1, Confidence::Possible)]);
        }
        for hits in operation_hits(&graph, "/users/me") {
            assert_eq!(hits, [(2, Confidence::Possible)]);
        }
        assert_eq!(graph.transitive_dependent_repos("api"), ["app"]);
    }

    // Distingue una asociación posible de la evidencia de tipos que la originó.
    #[test]
    fn preserves_type_evidence_when_a_dynamic_call_only_partially_matches() {
        let mut graph = DependencyGraph::new();
        graph.add_endpoint("api", HttpMethod::Get, "/users/me");
        graph.link_reads_field_with_confidence(
            "app",
            "api.ts",
            "rev",
            "api",
            HttpMethod::Get,
            "/users/:id",
            "name",
            2,
            "name: string",
            "api.ts",
            1,
            Confidence::Likely,
        );

        let hits = graph.consumers_of_field("api", HttpMethod::Get, "/users/me", "name");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].confidence, Confidence::Possible);
        assert_eq!(hits[0].source_confidence, Confidence::Likely);
    }

    // Comprueba que al agotar el límite una ruta quede sin cobertura demostrada.
    #[test]
    fn keeps_the_candidate_when_optional_coverage_exceeds_the_state_budget() {
        let suffix = "/item".repeat(MAX_ROUTE_COVERAGE_STATES);
        let pattern = format!("/x/*?{suffix}");
        let candidate = format!("/x{suffix}");

        assert!(!route_covers(&pattern, &candidate, false));
        assert!(route_covers("/x", &pattern, true));
    }

    #[test]
    fn matches_wildcard_and_concrete_literal_segments() {
        assert!(routes_match("/customers/*", "/customers/*"));
        assert!(routes_match("/customers/*", "/customers/1024"));
        assert!(!routes_match("/customers/*", "/orders/1024"));
        assert!(!routes_match("/customers/*", "/customers/1024/orders"));
    }

    #[test]
    fn matches_optional_wildcard_segments_when_omitted_or_present() {
        assert!(routes_match("/api/corp/*/*?", "/api/corp/loginCorp"));
        assert!(routes_match("/api/corp/*/*?", "/api/corp/loginCorp/123"));
        assert!(!routes_match("/api/corp/*/*?", "/api/corp"));
        assert!(!routes_match(
            "/api/corp/*/*?",
            "/api/corp/loginCorp/123/extra"
        ));

        assert!(routes_match("/api/web/*/*?/*?", "/api/web/getRateCars"));
        assert!(routes_match("/api/web/*/*?/*?", "/api/web/getRateCars/1"));
        assert!(routes_match("/api/web/*/*?/*?", "/api/web/getRateCars/1/2"));
        assert!(!routes_match(
            "/api/web/*/*?/*?",
            "/api/web/getRateCars/1/2/3"
        ));
    }

    #[test]
    fn queries_reverse_field_consumers_and_handles_cycles_safely() {
        let mut graph = DependencyGraph::new();
        graph.add_schema_field(
            "customer-service",
            HttpMethod::Get,
            "/customers/:id",
            "fullName",
        );
        graph.link_consumes(
            "billing-service",
            "src/invoices/customer.ts",
            "rev-1",
            "customer-service",
            HttpMethod::Get,
            "/customers/1024",
            2,
            "fetch(...)",
        );
        graph.link_reads_field(
            "billing-service",
            "src/invoices/customer.ts",
            "rev-1",
            "customer-service",
            HttpMethod::Get,
            "/customers/1024",
            "fullName",
            4,
            "customer.fullName",
            2,
        );

        // Introduce a cycle: customer-service also consumes billing-service.
        graph.link_consumes(
            "customer-service",
            "src/sync.ts",
            "rev-1",
            "billing-service",
            HttpMethod::Get,
            "/invoices/:id",
            10,
            "fetch(...)",
        );

        let hits = graph.consumers_of_field(
            "customer-service",
            HttpMethod::Get,
            "/customers/:id",
            "fullName",
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "billing-service");
        assert_eq!(hits[0].line, 4);

        let transitive = graph.transitive_dependent_repos("customer-service");
        assert_eq!(transitive, vec!["billing-service"]);
    }
}
