use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use laine_core::model::HttpMethod;
use laine_core::routes::{
    ExtractedRoute, RouteExtraction, RouteMountGraph, RouteOperation, RouteTarget,
    normalize_route_path,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentExpression, AssignmentTarget, BindingPattern, CallExpression, Declaration,
    ExportDeclaration, ExportDefaultDeclaration, ExportDefaultDeclarationKind,
    ExportFromDeclaration, ExportNamedDeclaration, Expression, Function, ImportDeclaration,
    ImportDeclarationSpecifier, ImportOrExportKind, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::ScopeFlags;
use oxc_span::SourceType;

use crate::ModuleResolver;

/// Extracts Express routes from one file using the snapshot mount resolver.
pub fn extract_express_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Result<Vec<ExtractedRoute>, Vec<String>> {
    let sources = BTreeMap::from([(file_path.to_owned(), source.to_owned())]);
    let resolver = ModuleResolver::from_git_snapshot(&[file_path.to_owned()], &BTreeMap::new());
    let (mut extraction, mut parse_errors) =
        extract_express_snapshot(service, &sources, &resolver, mount_prefix);
    if let Some(errors) = parse_errors.remove(file_path) {
        return Err(errors);
    }
    if let Some(lines) = extraction.unresolved_mounts.remove(file_path) {
        return Err(lines
            .into_iter()
            .map(|line| format!("Unresolved Express mount at {file_path}:{line}"))
            .collect());
    }
    Ok(extraction.routes)
}

/// Collects Express modules from a Git snapshot before resolving route paths.
pub fn extract_express_snapshot(
    service: &str,
    sources: &BTreeMap<String, String>,
    resolver: &ModuleResolver,
    mount_prefix: Option<&str>,
) -> (RouteExtraction, BTreeMap<String, Vec<String>>) {
    let mut modules = BTreeMap::new();
    let mut parse_errors = BTreeMap::new();
    let mut graph = RouteMountGraph::default();
    for (file, source) in sources {
        let allocator = Allocator::default();
        let source_type =
            SourceType::from_path(Path::new(file)).unwrap_or_else(|_| SourceType::ts());
        let parsed = Parser::new(&allocator, source, source_type).parse();
        if !parsed.diagnostics.is_empty() {
            parse_errors.insert(
                file.clone(),
                parsed
                    .diagnostics
                    .into_iter()
                    .map(|e| e.to_string())
                    .collect(),
            );
            continue;
        }
        let mut collector = ExpressCollector {
            source,
            line_starts: build_line_starts(source),
            module: ExpressModule::default(),
        };
        collector.visit_program(&parsed.program);
        for name in &collector.module.routers {
            graph.add_router((file.clone(), name.clone()), Some(String::new()), 0);
        }
        for name in &collector.module.apps {
            graph.add_application((file.clone(), name.clone()));
        }
        modules.insert(file.clone(), collector.module);
    }

    for (file, module) in &modules {
        for (owner, method, path, line, snippet) in &module.operations {
            if let Some(target) = resolve_target(&modules, resolver, file, owner) {
                graph.add_operation(
                    target,
                    RouteOperation {
                        method: *method,
                        path: path.clone(),
                        file: file.clone(),
                        line: *line,
                        snippet: snippet.clone(),
                    },
                );
            }
        }
        for mount in &module.mounts {
            let parent = mount
                .parent
                .as_deref()
                .and_then(|owner| resolve_target(&modules, resolver, file, owner));
            let known_child = |arg: &MountArgument| match arg {
                MountArgument::Reference(name) => resolve_target(&modules, resolver, file, name),
                _ => None,
            };
            if parent.is_none()
                && !mount
                    .parent
                    .as_deref()
                    .is_some_and(looks_like_express_target)
                && !mount.arguments.iter().any(|arg| known_child(arg).is_some())
            {
                continue;
            }
            let mut args = mount.arguments.as_slice();
            let prefix = match args {
                [MountArgument::Path(path), rest @ ..] => {
                    args = rest;
                    path.as_str()
                }
                [first, ..] if known_child(first).is_some() || module.is_middleware(first) => "",
                [_, _, ..] => {
                    // Express overloads use(path, ...handlers) and use(...handlers).
                    graph.mark_unresolved(file, mount.line);
                    continue;
                }
                _ => "",
            };
            for arg in args {
                if let Some(child) = known_child(arg)
                    && let Some(parent) = &parent
                {
                    graph.add_mount(parent.clone(), child, prefix.to_owned(), file, mount.line);
                } else if !module.is_middleware(arg) {
                    graph.mark_unresolved(file, mount.line);
                }
            }
        }
    }
    (
        graph.resolve(service, mount_prefix, normalize_route_path),
        parse_errors,
    )
}

#[derive(Default)]
struct ExpressModule {
    apps: BTreeSet<String>,
    routers: BTreeSet<String>,
    middleware: BTreeSet<String>,
    imports: BTreeMap<String, Import>,
    exports: BTreeMap<String, Export>,
    mounts: Vec<PendingMount>,
    operations: Vec<(String, HttpMethod, String, usize, String)>,
}

struct Import {
    source: String,
    name: Option<String>,
}

enum Export {
    Local(String),
    From(Import),
}

struct PendingMount {
    parent: Option<String>,
    line: usize,
    arguments: Vec<MountArgument>,
}

enum MountArgument {
    Path(String),
    Reference(String),
    Dynamic,
    Middleware,
}

impl ExpressModule {
    /// Recognizes statically defined middleware in an Express handler list.
    fn is_middleware(&self, argument: &MountArgument) -> bool {
        match argument {
            MountArgument::Reference(reference) => self.middleware.contains(reference),
            MountArgument::Middleware => true,
            MountArgument::Dynamic | MountArgument::Path(_) => false,
        }
    }
}

/// Follows import and export bindings until a route owner is found.
fn resolve_target(
    modules: &BTreeMap<String, ExpressModule>,
    resolver: &ModuleResolver,
    file: &str,
    reference: &str,
) -> Option<RouteTarget> {
    let mut file = file.to_owned();
    let mut reference = reference.to_owned();
    let mut exported = false;
    let mut seen = BTreeSet::new();
    while seen.insert((file.clone(), reference.clone(), exported)) {
        let module = modules.get(&file)?;
        let (import, qualified) = if exported {
            match module.exports.get(&reference)? {
                Export::Local(name) => {
                    reference = name.clone();
                    exported = false;
                    continue;
                }
                Export::From(import) => (import, None),
            }
        } else {
            let (root, member) = reference
                .split_once('.')
                .map_or((reference.as_str(), None), |(root, member)| {
                    (root, Some(member))
                });
            if let Some(import) = module.imports.get(root) {
                (import, member)
            } else {
                return (module.apps.contains(&reference)
                    || module.routers.contains(&reference)
                    || (looks_like_express_target(&reference)
                        && module.operations.iter().any(|op| op.0 == reference)))
                .then_some((file, reference));
            }
        };
        reference = match (import.name.as_deref(), qualified) {
            (Some(name), None) | (None, Some(name)) => name.to_owned(),
            _ => return None,
        };
        file = resolver.resolve_import(&file, &import.source)?;
        exported = true;
    }
    None
}

struct ExpressCollector<'s> {
    source: &'s str,
    line_starts: Vec<usize>,
    module: ExpressModule,
}

impl ExpressCollector<'_> {
    /// Finds the original line and snippet for a byte offset.
    fn line_and_snippet(&self, byte_offset: u32) -> (usize, String) {
        let offset = byte_offset as usize;
        let line = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        };
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(self.source.len());
        (line, self.source[start..end].trim().to_owned())
    }
}

impl<'a> Visit<'a> for ExpressCollector<'_> {
    /// Collects value imports with their local aliases.
    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'a>) {
        if decl.import_kind == ImportOrExportKind::Type {
            return;
        }
        let Some(source) = decl.source.value.as_str() else {
            return;
        };
        for specifier in decl.specifiers.iter().flatten() {
            let (local, name) = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(spec)
                    if spec.import_kind == ImportOrExportKind::Value =>
                {
                    (
                        spec.local.name.as_str(),
                        Some(spec.imported.name().to_string()),
                    )
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                    (spec.local.name.as_str(), Some("default".to_owned()))
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                    (spec.local.name.as_str(), None)
                }
                _ => continue,
            };
            self.module.imports.insert(
                local.to_owned(),
                Import {
                    source: source.to_owned(),
                    name,
                },
            );
        }
    }

    /// Collects bindings declared with export const, let or var.
    fn visit_export_declaration(&mut self, decl: &ExportDeclaration<'a>) {
        if let Declaration::VariableDeclaration(variables) = &decl.declaration {
            for variable in &variables.declarations {
                if let BindingPattern::BindingIdentifier(id) = &variable.id {
                    let name = id.name.to_string();
                    self.module
                        .exports
                        .insert(name.clone(), Export::Local(name));
                }
            }
        }
        walk::walk_export_declaration(self, decl);
    }

    /// Collects aliases exported from this module's local bindings.
    fn visit_export_named_declaration(&mut self, decl: &ExportNamedDeclaration<'a>) {
        if decl.export_kind == ImportOrExportKind::Type {
            return;
        }
        for spec in &decl.specifiers {
            if spec.export_kind == ImportOrExportKind::Value {
                self.module.exports.insert(
                    spec.exported.name().to_string(),
                    Export::Local(spec.local.name().to_string()),
                );
            }
        }
        walk::walk_export_named_declaration(self, decl);
    }

    /// Records explicit re-exports for router barrel files.
    fn visit_export_from_declaration(&mut self, decl: &ExportFromDeclaration<'a>) {
        if decl.export_kind == ImportOrExportKind::Type {
            return;
        }
        let Some(source) = decl.source.value.as_str() else {
            return;
        };
        for spec in &decl.specifiers {
            if spec.export_kind == ImportOrExportKind::Value {
                self.module.exports.insert(
                    spec.exported.name().to_string(),
                    Export::From(Import {
                        source: source.to_owned(),
                        name: Some(spec.local.name().to_string()),
                    }),
                );
            }
        }
    }

    /// Tracks default exports of named router objects.
    fn visit_export_default_declaration(&mut self, decl: &ExportDefaultDeclaration<'a>) {
        if let ExportDefaultDeclarationKind::Identifier(id) = &decl.declaration {
            self.module
                .exports
                .insert("default".to_owned(), Export::Local(id.name.to_string()));
        }
        walk::walk_export_default_declaration(self, decl);
    }

    /// Tracks CommonJS modules that export a router directly.
    fn visit_assignment_expression(&mut self, assign: &AssignmentExpression<'a>) {
        if let AssignmentTarget::StaticMemberExpression(member) = &assign.left
            && member.property.name == "exports"
            && matches!(&member.object, Expression::Identifier(id) if id.name == "module")
            && let Expression::Identifier(id) = &assign.right
        {
            self.module
                .exports
                .insert("default".to_owned(), Export::Local(id.name.to_string()));
        }
        walk::walk_assignment_expression(self, assign);
    }

    /// Recognizes Express constructors, CommonJS imports and inline middleware bindings.
    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(id) = &decl.id {
            let name = id.name.to_string();
            match &decl.init {
                Some(Expression::CallExpression(call)) => {
                    let callee = static_reference(&call.callee).unwrap_or_default();
                    let (root, member) = callee.split_once('.').unwrap_or((&callee, ""));
                    let express_import = self
                        .module
                        .imports
                        .get(root)
                        .filter(|import| import.source == "express");
                    if root == "require"
                        && let Some(source) = call.arguments.first().and_then(literal_argument)
                    {
                        self.module.imports.insert(
                            name,
                            Import {
                                source: source.to_owned(),
                                name: Some("default".to_owned()),
                            },
                        );
                    } else if (member == "Router"
                        && (root == "express" || express_import.is_some()))
                        || (member.is_empty()
                            && (root == "Router"
                                || express_import.is_some_and(|import| {
                                    import.name.as_deref() == Some("Router")
                                })))
                    {
                        self.module.routers.insert(name);
                    } else if member.is_empty() && (root == "express" || express_import.is_some()) {
                        self.module.apps.insert(name);
                    }
                }
                Some(
                    Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_),
                ) => {
                    self.module.middleware.insert(name);
                }
                _ => {}
            }
        }
        walk::walk_variable_declarator(self, decl);
    }

    /// Records named functions so Express middleware can precede a router in use().
    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &function.id {
            self.module.middleware.insert(id.name.to_string());
        }
        walk::walk_function(self, function, flags);
    }

    /// Records route operations and mount calls with source evidence.
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = &call.callee {
            if member.property.name == "use" {
                let parent = static_reference(&member.object);
                let (line, _) = self.line_and_snippet(call.span.start);
                let arguments = call.arguments.iter().map(mount_argument).collect();
                self.module.mounts.push(PendingMount {
                    parent,
                    line,
                    arguments,
                });
            } else if let Some(method) = HttpMethod::parse(member.property.name.as_str()) {
                let route = if let Expression::CallExpression(inner) = &member.object {
                    extract_chained_route(inner)
                } else {
                    static_reference(&member.object).zip(
                        call.arguments
                            .first()
                            .and_then(literal_argument)
                            .map(str::to_owned),
                    )
                };
                if let Some((owner, path)) = route {
                    let (line, snippet) = self.line_and_snippet(call.span.start);
                    self.module
                        .operations
                        .push((owner, method, path, line, snippet));
                }
            }
        }
        walk::walk_call_expression(self, call);
    }
}

/// Reads an identifier or a namespace member used as a route owner.
fn static_reference(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::StaticMemberExpression(member) => {
            let Expression::Identifier(namespace) = &member.object else {
                return None;
            };
            Some(format!("{}.{}", namespace.name, member.property.name))
        }
        _ => None,
    }
}

/// Separates static paths and handler references from dynamic expressions.
fn mount_argument(argument: &oxc_ast::ast::Argument<'_>) -> MountArgument {
    if let Some(path) = literal_argument(argument) {
        return MountArgument::Path(path.to_owned());
    }
    match argument.as_expression() {
        Some(Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)) => {
            MountArgument::Middleware
        }
        Some(Expression::CallExpression(call))
            if static_reference(&call.callee).is_some_and(|name| {
                matches!(
                    name.as_str(),
                    "express.json"
                        | "express.static"
                        | "express.urlencoded"
                        | "express.raw"
                        | "express.text"
                )
            }) =>
        {
            MountArgument::Middleware
        }
        Some(expr) => {
            static_reference(expr).map_or(MountArgument::Dynamic, MountArgument::Reference)
        }
        None => MountArgument::Dynamic,
    }
}

/// Reads string paths and module specifiers shared by routes and mounts.
fn literal_argument<'a>(argument: &'a oxc_ast::ast::Argument<'_>) -> Option<&'a str> {
    match argument.as_expression()? {
        Expression::StringLiteral(path) => path.value.as_str(),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => template
            .quasis
            .first()?
            .value
            .cooked
            .as_ref()
            .and_then(|value| value.as_str()),
        _ => None,
    }
}

/// Recognizes conventional route owners when a constructor is outside the analyzed scope.
fn looks_like_express_target(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower != "axios"
        && (lower == "app"
            || lower == "server"
            || lower.contains("router")
            || lower.ends_with("routes")
            || lower.ends_with("api"))
}

/// Unwraps all methods chained on the same route().
fn extract_chained_route(mut call: &CallExpression<'_>) -> Option<(String, String)> {
    loop {
        let Expression::StaticMemberExpression(member) = &call.callee else {
            return None;
        };
        if member.property.name == "route" {
            return Some((
                static_reference(&member.object)?,
                literal_argument(call.arguments.first()?)?.to_owned(),
            ));
        }
        let Expression::CallExpression(previous) = &member.object else {
            return None;
        };
        call = previous;
    }
}

/// Indexes line starts so route evidence can be recovered from AST spans.
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
