use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use petgraph::Direction;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::model::HttpMethod;
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
    },
    ReadsField {
        revision: String,
        line: usize,
        snippet: String,
        call_line: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldConsumerHit {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
    pub call_line: usize,
    pub matched_field: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConsumerHit {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
}

/// Directed dependency graph built on `petgraph::DiGraph` with automatic node deduplication.
#[derive(Debug, Default)]
pub struct DependencyGraph {
    graph: DiGraph<GraphNode, GraphEdge>,
    repos: HashMap<String, NodeIndex>,
    files: HashMap<(String, String), NodeIndex>,
    endpoints: HashMap<(String, HttpMethod, String), NodeIndex>,
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

    pub fn add_endpoint(
        &mut self,
        service: &str,
        method: HttpMethod,
        display_path: &str,
    ) -> NodeIndex {
        let canonical_path = canonical_route_key(display_path);
        let key = (service.to_string(), method, canonical_path.clone());
        if let Some(&idx) = self.endpoints.get(&key) {
            return idx;
        }
        let repo_idx = self.add_repository(service);
        let ep_idx = self.graph.add_node(GraphNode::Endpoint {
            service: service.to_string(),
            method,
            canonical_path,
            display_path: display_path.to_string(),
        });
        self.endpoints.insert(key, ep_idx);
        self.graph.add_edge(repo_idx, ep_idx, GraphEdge::Exposes);
        ep_idx
    }

    pub fn add_schema_field(
        &mut self,
        service: &str,
        method: HttpMethod,
        display_path: &str,
        field_path: &str,
    ) -> NodeIndex {
        let canonical_path = canonical_route_key(display_path);
        let key = (
            service.to_string(),
            method,
            canonical_path.clone(),
            field_path.to_string(),
        );
        if let Some(&idx) = self.fields.get(&key) {
            return idx;
        }
        let ep_idx = self.add_endpoint(service, method, display_path);
        let field_idx = self.graph.add_node(GraphNode::SchemaField {
            service: service.to_string(),
            method,
            canonical_path,
            field_path: field_path.to_string(),
        });
        self.fields.insert(key, field_idx);
        self.graph
            .add_edge(ep_idx, field_idx, GraphEdge::ReturnsField);
        field_idx
    }

    #[allow(clippy::too_many_arguments)]
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
        let ep_idx = self
            .find_matching_endpoint_node(target_service, method, consumer_path)
            .unwrap_or_else(|| self.add_endpoint(target_service, method, consumer_path));

        self.graph.add_edge(
            file_idx,
            ep_idx,
            GraphEdge::Consumes {
                revision: revision.to_string(),
                line,
                snippet: snippet.to_string(),
            },
        );
        ep_idx
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
        let file_idx = self.add_file(consumer_repo, consumer_file);

        // Attach to the producer's canonical endpoint path if already registered.
        let resolved_display_path = self
            .find_matching_endpoint_node(target_service, method, consumer_path)
            .and_then(|idx| match &self.graph[idx] {
                GraphNode::Endpoint { display_path, .. } => Some(display_path.clone()),
                _ => None,
            })
            .unwrap_or_else(|| consumer_path.to_string());

        let field_idx =
            self.add_schema_field(target_service, method, &resolved_display_path, field_path);

        self.graph.add_edge(
            file_idx,
            field_idx,
            GraphEdge::ReadsField {
                revision: revision.to_string(),
                line,
                snippet: snippet.to_string(),
                call_line,
            },
        );
    }

    /// Finds all consumer file locations reading `changed_field` (or nested children like `address.city`
    /// when `address` changes) on a matching endpoint.
    pub fn consumers_of_field(
        &self,
        service: &str,
        method: HttpMethod,
        producer_path: &str,
        changed_field: &str,
    ) -> Vec<FieldConsumerHit> {
        let producer_canonical = canonical_route_key(producer_path);
        let mut hits = Vec::new();
        let mut dedup = HashSet::new();

        for (&(ref svc, m, ref ep_canonical, ref field_path), &field_idx) in &self.fields {
            if svc != service
                || m != method
                || !routes_match(&producer_canonical, ep_canonical)
                || !field_matches(changed_field, field_path)
            {
                continue;
            }

            for edge in self.graph.edges_directed(field_idx, Direction::Incoming) {
                if let GraphEdge::ReadsField {
                    revision,
                    line,
                    snippet,
                    call_line,
                } = edge.weight()
                    && let GraphNode::File { repo, path } = &self.graph[edge.source()]
                    && dedup.insert((repo.clone(), path.clone(), *line, field_path.clone()))
                {
                    hits.push(FieldConsumerHit {
                        repo: repo.clone(),
                        revision: revision.clone(),
                        file: path.clone(),
                        line: *line,
                        snippet: snippet.clone(),
                        call_line: *call_line,
                        matched_field: field_path.clone(),
                    });
                }
            }
        }

        hits.sort_by(|a, b| {
            (&a.repo, &a.file, a.line, &a.matched_field).cmp(&(
                &b.repo,
                &b.file,
                b.line,
                &b.matched_field,
            ))
        });
        hits
    }

    /// Finds all consumer file locations calling an endpoint (used when an entire endpoint is removed).
    pub fn consumers_of_endpoint(
        &self,
        service: &str,
        method: HttpMethod,
        producer_path: &str,
    ) -> Vec<EndpointConsumerHit> {
        let producer_canonical = canonical_route_key(producer_path);
        let mut hits = Vec::new();
        let mut dedup = HashSet::new();

        for (&(ref svc, m, ref ep_canonical), &ep_idx) in &self.endpoints {
            if svc != service || m != method || !routes_match(&producer_canonical, ep_canonical) {
                continue;
            }

            for edge in self.graph.edges_directed(ep_idx, Direction::Incoming) {
                if let GraphEdge::Consumes {
                    revision,
                    line,
                    snippet,
                } = edge.weight()
                    && let GraphNode::File { repo, path } = &self.graph[edge.source()]
                    && dedup.insert((repo.clone(), path.clone(), *line))
                {
                    hits.push(EndpointConsumerHit {
                        repo: repo.clone(),
                        revision: revision.clone(),
                        file: path.clone(),
                        line: *line,
                        snippet: snippet.clone(),
                    });
                }
            }
        }

        hits.sort_by(|a, b| (&a.repo, &a.file, a.line).cmp(&(&b.repo, &b.file, b.line)));
        hits
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

    fn find_matching_endpoint_node(
        &self,
        service: &str,
        method: HttpMethod,
        consumer_path: &str,
    ) -> Option<NodeIndex> {
        let consumer_canonical = canonical_route_key(consumer_path);
        if let Some(&exact) =
            self.endpoints
                .get(&(service.to_string(), method, consumer_canonical.clone()))
        {
            return Some(exact);
        }

        self.endpoints
            .iter()
            .find_map(|((svc, m, ep_canonical), &idx)| {
                if svc == service && *m == method && routes_match(ep_canonical, &consumer_canonical)
                {
                    Some(idx)
                } else {
                    None
                }
            })
    }
}

/// Checks whether two canonical route keys match segment-by-segment, treating `*` as a wildcard
/// that matches both parameterized segments (`:id`) and concrete literals (`1024`).
pub fn routes_match(producer_canonical: &str, consumer_canonical: &str) -> bool {
    let prod_segs: Vec<&str> = producer_canonical
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let cons_segs: Vec<&str> = consumer_canonical
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    if prod_segs.len() != cons_segs.len() {
        return false;
    }

    prod_segs
        .iter()
        .zip(cons_segs.iter())
        .all(|(p, c)| *p == "*" || *c == "*" || p == c)
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

    #[test]
    fn matches_wildcard_and_concrete_literal_segments() {
        assert!(routes_match("/customers/*", "/customers/*"));
        assert!(routes_match("/customers/*", "/customers/1024"));
        assert!(!routes_match("/customers/*", "/orders/1024"));
        assert!(!routes_match("/customers/*", "/customers/1024/orders"));
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
