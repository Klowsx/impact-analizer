use std::collections::HashMap;
use std::path::Path;

use laine_core::model::{EndpointId, HttpMethod};
use laine_core::routes::{ExtractedRoute, normalize_route_path};
use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Expression};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// Extracts Express HTTP routes (`app.get/post/put/patch/delete`, `router.get/...`,
/// `router.route('/x').get(...).delete(...)`, and `app.use('/prefix', router)`)
/// from a TypeScript or JavaScript source file using `oxc`.
pub fn extract_express_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Result<Vec<ExtractedRoute>, Vec<String>> {
    let allocator = Allocator::default();
    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if !parsed.diagnostics.is_empty() {
        return Err(parsed
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    // Pass 1: discover `app.use('/prefix', subRouter)` mounts in this file.
    let mut mount_collector = MountCollector {
        router_prefixes: HashMap::new(),
    };
    mount_collector.visit_program(&parsed.program);

    // Pass 2: extract route definitions with resolved mount prefixes.
    let mut route_collector = ExpressRouteCollector {
        service,
        file_path,
        source,
        line_starts: build_line_starts(source),
        mount_prefix,
        router_prefixes: mount_collector.router_prefixes,
        routes: Vec::new(),
    };
    route_collector.visit_program(&parsed.program);

    Ok(route_collector.routes)
}

struct MountCollector {
    router_prefixes: HashMap<String, String>,
}

impl<'a> Visit<'a> for MountCollector {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = &call.callee
            && member.property.name == "use"
            && call.arguments.len() >= 2
            && let Some(Expression::StringLiteral(prefix_lit)) = call.arguments[0].as_expression()
            && let Some(prefix) = prefix_lit.value.as_str()
            && let Some(Expression::Identifier(router_id)) = call.arguments[1].as_expression()
        {
            self.router_prefixes
                .insert(router_id.name.to_string(), prefix.to_string());
        }
        walk::walk_call_expression(self, call);
    }
}

struct ExpressRouteCollector<'s> {
    service: &'s str,
    file_path: &'s str,
    source: &'s str,
    line_starts: Vec<usize>,
    mount_prefix: Option<&'s str>,
    router_prefixes: HashMap<String, String>,
    routes: Vec<ExtractedRoute>,
}

impl<'s> ExpressRouteCollector<'s> {
    fn line_and_snippet(&self, byte_offset: u32) -> (usize, String) {
        let offset = byte_offset as usize;
        let line = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        };
        let snippet = if line == 0 || line > self.line_starts.len() {
            String::new()
        } else {
            let start = self.line_starts[line - 1];
            let end = self
                .line_starts
                .get(line)
                .copied()
                .unwrap_or(self.source.len());
            self.source[start..end].trim().to_string()
        };
        (line, snippet)
    }

    fn push_route(
        &mut self,
        router_name: Option<&str>,
        method: HttpMethod,
        raw_path: &str,
        offset: u32,
    ) {
        let mut prefixes = Vec::new();
        if let Some(mp) = self.mount_prefix {
            prefixes.push(mp);
        }
        if let Some(name) = router_name
            && let Some(rp) = self.router_prefixes.get(name)
        {
            prefixes.push(rp.as_str());
        }

        let normalized_path = normalize_route_path(&prefixes, raw_path);
        let (line, snippet) = self.line_and_snippet(offset);

        self.routes.push(ExtractedRoute {
            endpoint: EndpointId {
                service: self.service.to_string(),
                method,
                normalized_path,
            },
            file: self.file_path.to_string(),
            line,
            snippet,
        });
    }
}

impl<'a> Visit<'a> for ExpressRouteCollector<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = &call.callee
            && let Some(method) = HttpMethod::parse(member.property.name.as_str())
        {
            match &member.object {
                // `app.get('/customers/:id', handler)` or `customerRouter.post('/', handler)`
                Expression::Identifier(obj) if looks_like_express_target(obj.name.as_str()) => {
                    if let Some(first) = call.arguments.first()
                        && let Some(Expression::StringLiteral(path_lit)) = first.as_expression()
                        && let Some(path) = path_lit.value.as_str()
                    {
                        self.push_route(Some(obj.name.as_str()), method, path, call.span().start);
                    }
                }
                // `router.route('/customers/:id').get(h1).delete(h2)`
                Expression::CallExpression(inner_call) => {
                    if let Some((router_name, path)) = extract_chained_route(inner_call) {
                        self.push_route(router_name.as_deref(), method, &path, call.span().start);
                    }
                }
                _ => {}
            }
        }
        walk::walk_call_expression(self, call);
    }
}

fn looks_like_express_target(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "axios"
        && (lower == "app"
            || lower == "server"
            || lower.contains("router")
            || lower.ends_with("routes")
            || lower.ends_with("api"))
}

fn extract_chained_route(call: &CallExpression<'_>) -> Option<(Option<String>, String)> {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return None;
    };
    if member.property.name == "route" {
        let router_name = match &member.object {
            Expression::Identifier(id) => Some(id.name.to_string()),
            _ => None,
        };
        let Expression::StringLiteral(path_lit) = call.arguments.first()?.as_expression()? else {
            return None;
        };
        return Some((router_name, path_lit.value.as_str()?.to_string()));
    }
    // Support multi-method chains: `router.route('/x').get(a).delete(b)`
    if let Expression::CallExpression(prev_call) = &member.object {
        return extract_chained_route(prev_call);
    }
    None
}

fn build_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_express_routes_with_mount_prefixes_and_chained_route() {
        let code = r#"
import express from "express";

const app = express();
const customerRouter = express.Router();

customerRouter.get("/:id", getCustomer);
customerRouter.route("/:id/orders").get(listOrders).post(createOrder);

app.use("/api/v1/customers", customerRouter);
app.delete("/health/cache", clearCache);
"#;

        let routes = extract_express_routes("customer-service", "src/server.ts", code, None)
            .expect("express TS should parse");

        let summary: Vec<(HttpMethod, &str)> = routes
            .iter()
            .map(|r| (r.endpoint.method, r.endpoint.normalized_path.as_str()))
            .collect();

        assert_eq!(
            summary,
            vec![
                (HttpMethod::Get, "/api/v1/customers/:id"),
                (HttpMethod::Post, "/api/v1/customers/:id/orders"),
                (HttpMethod::Get, "/api/v1/customers/:id/orders"),
                (HttpMethod::Delete, "/health/cache"),
            ]
        );
    }
}
