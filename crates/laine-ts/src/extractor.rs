use std::collections::{HashMap, HashSet};
use std::path::Path;

use laine_core::config::ServiceConfig;
use laine_core::model::HttpMethod;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, ChainElement, Expression, ObjectPropertyKind,
    PropertyKey, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::url::resolve_url_against_services;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpClientKind {
    Fetch,
    Axios,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedHttpCall {
    pub id: usize,
    pub client: HttpClientKind,
    pub service: String,
    pub method: HttpMethod,
    pub normalized_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedFieldRead {
    pub call_id: usize,
    pub field_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedUnresolvedCall {
    pub client: HttpClientKind,
    pub line: usize,
    pub snippet: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FileExtraction {
    pub file_path: String,
    pub calls: Vec<ExtractedHttpCall>,
    pub field_reads: Vec<ExtractedFieldRead>,
    pub unresolved_calls: Vec<ExtractedUnresolvedCall>,
}

/// Extracts HTTP consumer calls (`fetch`, `axios`), field reads (`customer.fullName`,
/// `response.data.fullName`, destructuring, local aliases), and unresolved dynamic URLs
/// from a TypeScript or JavaScript file.
pub fn extract_file(
    file_path: &str,
    source: &str,
    services: &[ServiceConfig],
) -> Result<FileExtraction, Vec<String>> {
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

    let semantic = SemanticBuilder::new().build(&parsed.program);
    if !semantic.diagnostics.is_empty() {
        return Err(semantic
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    let mut extractor = FileExtractor::new(file_path, source, services);
    extractor.visit_program(&parsed.program);
    Ok(extractor.finish())
}

#[derive(Debug, Clone)]
enum VarBinding {
    StringLiteral(String),
    FetchResponse(usize),
    AxiosResponse(usize),
    JsonPayload { call_id: usize, prefix: String },
}

#[derive(Debug, Clone)]
enum UrlEval {
    Resolved(String),
    Dynamic(String),
}

struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (idx, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    fn line_number(&self, byte_offset: u32) -> usize {
        let offset = byte_offset as usize;
        match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }

    fn line_snippet<'a>(&self, source: &'a str, line: usize) -> &'a str {
        if line == 0 || line > self.line_starts.len() {
            return "";
        }
        let start = self.line_starts[line - 1];
        let end = self.line_starts.get(line).copied().unwrap_or(source.len());
        source[start..end].trim()
    }
}

struct FileExtractor<'s> {
    file_path: String,
    source: &'s str,
    line_index: LineIndex,
    services: &'s [ServiceConfig],
    scopes: Vec<HashMap<String, VarBinding>>,
    calls: Vec<ExtractedHttpCall>,
    field_reads: Vec<ExtractedFieldRead>,
    unresolved_calls: Vec<ExtractedUnresolvedCall>,
    seen_call_spans: HashSet<u32>,
    seen_member_spans: HashSet<u32>,
    seen_reads: HashSet<(usize, String, usize)>,
}

impl<'s> FileExtractor<'s> {
    fn new(file_path: &str, source: &'s str, services: &'s [ServiceConfig]) -> Self {
        Self {
            file_path: file_path.to_string(),
            source,
            line_index: LineIndex::new(source),
            services,
            scopes: vec![HashMap::new()],
            calls: Vec::new(),
            field_reads: Vec::new(),
            unresolved_calls: Vec::new(),
            seen_call_spans: HashSet::new(),
            seen_member_spans: HashSet::new(),
            seen_reads: HashSet::new(),
        }
    }

    fn finish(self) -> FileExtraction {
        FileExtraction {
            file_path: self.file_path,
            calls: self.calls,
            field_reads: self.field_reads,
            unresolved_calls: self.unresolved_calls,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn bind_var(&mut self, name: impl Into<String>, binding: VarBinding) {
        if let Some(current) = self.scopes.last_mut() {
            current.insert(name.into(), binding);
        }
    }

    fn lookup_var(&self, name: &str) -> Option<&VarBinding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn location(&self, span: Span) -> (usize, String) {
        let line = self.line_index.line_number(span.start);
        let snippet = self.line_index.line_snippet(self.source, line).to_string();
        (line, snippet)
    }

    fn record_field_read(&mut self, call_id: usize, field_path: String, span: Span) {
        if field_path.is_empty() {
            return;
        }
        let (line, snippet) = self.location(span);
        if self.seen_reads.insert((call_id, field_path.clone(), line)) {
            self.field_reads.push(ExtractedFieldRead {
                call_id,
                field_path,
                line,
                snippet,
            });
        }
    }

    fn classify_http_call(
        &self,
        call: &CallExpression<'_>,
    ) -> Option<(HttpClientKind, HttpMethod)> {
        match unwrap_expr(&call.callee) {
            Expression::Identifier(ident) if ident.name == "fetch" => {
                let method = extract_fetch_method(call).unwrap_or(HttpMethod::Get);
                Some((HttpClientKind::Fetch, method))
            }
            Expression::StaticMemberExpression(member) => {
                if let Expression::Identifier(obj) = unwrap_expr(&member.object)
                    && obj.name == "axios"
                    && let Some(method) = HttpMethod::parse(member.property.name.as_str())
                {
                    return Some((HttpClientKind::Axios, method));
                }
                None
            }
            _ => None,
        }
    }

    fn process_http_call(&mut self, call: &CallExpression<'_>) -> Option<(HttpClientKind, usize)> {
        let (client, method) = self.classify_http_call(call)?;
        if !self.seen_call_spans.insert(call.span.start) {
            return None;
        }

        let (line, snippet) = self.location(call.span);
        let Some(first_arg) = call.arguments.first() else {
            self.unresolved_calls.push(ExtractedUnresolvedCall {
                client,
                line,
                snippet,
                reason: "missing URL argument".to_string(),
            });
            return None;
        };

        let Some(url_expr) = first_arg.as_expression() else {
            self.unresolved_calls.push(ExtractedUnresolvedCall {
                client,
                line,
                snippet,
                reason: "spread argument cannot be resolved statically".to_string(),
            });
            return None;
        };

        match self.eval_url_expr(url_expr) {
            UrlEval::Resolved(url_str) => {
                if let Some(matched) = resolve_url_against_services(&url_str, self.services) {
                    let id = self.calls.len();
                    self.calls.push(ExtractedHttpCall {
                        id,
                        client,
                        service: matched.service,
                        method,
                        normalized_path: matched.normalized_path,
                        line,
                        snippet,
                    });
                    Some((client, id))
                } else {
                    None
                }
            }
            UrlEval::Dynamic(reason) => {
                self.unresolved_calls.push(ExtractedUnresolvedCall {
                    client,
                    line,
                    snippet,
                    reason,
                });
                None
            }
        }
    }

    fn eval_url_expr(&self, expr: &Expression<'_>) -> UrlEval {
        match unwrap_expr(expr) {
            Expression::StringLiteral(lit) => match lit.value.as_str() {
                Some(s) => UrlEval::Resolved(s.to_string()),
                None => UrlEval::Dynamic("non-UTF8 string literal".into()),
            },
            Expression::TemplateLiteral(tpl) => self.eval_template_url(tpl),
            Expression::Identifier(ident) => match self.lookup_var(ident.name.as_str()) {
                Some(VarBinding::StringLiteral(val)) => UrlEval::Resolved(val.clone()),
                _ => UrlEval::Dynamic(format!(
                    "dynamic URL variable `{}` cannot be resolved statically",
                    ident.name
                )),
            },
            Expression::BinaryExpression(bin) if bin.operator.as_str() == "+" => {
                let left = self.eval_url_expr(&bin.left);
                let right = self.eval_url_expr(&bin.right);
                match (left, right) {
                    (UrlEval::Resolved(l), UrlEval::Resolved(r)) => {
                        UrlEval::Resolved(format!("{l}{r}"))
                    }
                    (UrlEval::Resolved(l), UrlEval::Dynamic(_)) if l.ends_with('/') => {
                        UrlEval::Resolved(format!("{l}:param"))
                    }
                    _ => UrlEval::Dynamic(
                        "dynamic URL concatenation cannot be resolved statically".to_string(),
                    ),
                }
            }
            _ => UrlEval::Dynamic("dynamic URL expression cannot be resolved statically".into()),
        }
    }

    fn eval_template_url(&self, tpl: &TemplateLiteral<'_>) -> UrlEval {
        let mut out = String::new();

        for (idx, quasi) in tpl.quasis.iter().enumerate() {
            let chunk = quasi
                .value
                .cooked
                .as_ref()
                .and_then(|c| c.as_str())
                .unwrap_or_else(|| quasi.value.raw.as_str());
            out.push_str(chunk);

            if let Some(expr) = tpl.expressions.get(idx) {
                match unwrap_expr(expr) {
                    Expression::StringLiteral(lit) => {
                        if let Some(s) = lit.value.as_str() {
                            out.push_str(s);
                        }
                    }
                    Expression::Identifier(ident) => {
                        if let Some(VarBinding::StringLiteral(val)) =
                            self.lookup_var(ident.name.as_str())
                        {
                            out.push_str(val);
                        } else if out.is_empty() {
                            return UrlEval::Dynamic(format!(
                                "dynamic URL prefix `${{{}}}` cannot be resolved statically",
                                ident.name
                            ));
                        } else {
                            out.push(':');
                            out.push_str(ident.name.as_str());
                        }
                    }
                    _ => {
                        if out.is_empty() {
                            return UrlEval::Dynamic(
                                "dynamic template URL prefix cannot be resolved statically".into(),
                            );
                        }
                        out.push_str(":param");
                    }
                }
            }
        }

        UrlEval::Resolved(out)
    }

    fn resolve_binding_from_expr(&mut self, expr: &Expression<'_>) -> Option<VarBinding> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::StringLiteral(lit) => lit
                .value
                .as_str()
                .map(|s| VarBinding::StringLiteral(s.to_string())),
            Expression::CallExpression(call) => {
                // Direct `fetch(...)` or `axios.get(...)`
                if let Some((client, call_id)) = self.process_http_call(call) {
                    return Some(match client {
                        HttpClientKind::Fetch => VarBinding::FetchResponse(call_id),
                        HttpClientKind::Axios => VarBinding::AxiosResponse(call_id),
                    });
                }

                // `res.json()` or `(await fetch(...)).json()`
                if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee)
                    && member.property.name == "json"
                    && let Some(VarBinding::FetchResponse(call_id)) =
                        self.resolve_binding_from_expr(&member.object)
                {
                    return Some(VarBinding::JsonPayload {
                        call_id,
                        prefix: String::new(),
                    });
                }
                None
            }
            Expression::Identifier(ident) => self.lookup_var(ident.name.as_str()).cloned(),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root_name, segments) = extract_member_chain(unwrapped)?;
                let root_binding = self.lookup_var(&root_name)?.clone();
                match root_binding {
                    VarBinding::AxiosResponse(call_id) => {
                        let first = segments.first()?;
                        if first != "data" {
                            return None;
                        }
                        let field_segments = &segments[1..];
                        if field_segments.is_empty() {
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: String::new(),
                            })
                        } else {
                            let field_path = field_segments.join(".");
                            self.record_field_read(call_id, field_path.clone(), unwrapped.span());
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: field_path,
                            })
                        }
                    }
                    VarBinding::JsonPayload { call_id, prefix } => {
                        let full_path = join_field_path(&prefix, &segments);
                        self.record_field_read(call_id, full_path.clone(), unwrapped.span());
                        Some(VarBinding::JsonPayload {
                            call_id,
                            prefix: full_path,
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn bind_pattern(&mut self, pattern: &BindingPattern<'_>, binding: VarBinding) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), binding);
            }
            BindingPattern::ObjectPattern(obj_pat) => {
                for prop in &obj_pat.properties {
                    let Some(key_name) = property_key_name(&prop.key) else {
                        continue;
                    };
                    match &binding {
                        VarBinding::AxiosResponse(call_id) if key_name == "data" => {
                            self.bind_pattern(
                                &prop.value,
                                VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: String::new(),
                                },
                            );
                        }
                        VarBinding::JsonPayload { call_id, prefix } => {
                            let field_path = join_field_path(prefix, &[key_name]);
                            self.record_field_read(*call_id, field_path.clone(), prop.span);
                            self.bind_pattern(
                                &prop.value,
                                VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: field_path,
                                },
                            );
                        }
                        _ => {}
                    }
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.bind_pattern(&assign.left, binding);
            }
            BindingPattern::ArrayPattern(_) => {}
        }
    }

    fn inspect_member_chain(&mut self, expr: &Expression<'_>) {
        let span = expr.span();
        if !self.seen_member_spans.insert(span.start) {
            return;
        }
        let Some((root_name, segments)) = extract_member_chain(expr) else {
            return;
        };
        let Some(root_binding) = self.lookup_var(&root_name).cloned() else {
            return;
        };

        match root_binding {
            VarBinding::JsonPayload { call_id, prefix } => {
                let full_path = join_field_path(&prefix, &segments);
                self.record_field_read(call_id, full_path, span);
            }
            VarBinding::AxiosResponse(call_id)
                if segments.first().map(String::as_str) == Some("data") && segments.len() > 1 =>
            {
                let full_path = segments[1..].join(".");
                self.record_field_read(call_id, full_path, span);
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for FileExtractor<'_> {
    fn visit_block_statement(&mut self, it: &oxc_ast::ast::BlockStatement<'a>) {
        self.push_scope();
        walk::walk_block_statement(self, it);
        self.pop_scope();
    }

    fn visit_function(&mut self, it: &oxc_ast::ast::Function<'a>, flags: oxc_semantic::ScopeFlags) {
        self.push_scope();
        walk::walk_function(self, it, flags);
        self.pop_scope();
    }

    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        self.push_scope();
        walk::walk_arrow_function_expression(self, it);
        self.pop_scope();
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(init) = &decl.init {
            if let Some(binding) = self.resolve_binding_from_expr(init) {
                self.bind_pattern(&decl.id, binding);
            }
            // Mark outer member chain span so walking `init` doesn't record inner sub-chains twice.
            if matches!(
                unwrap_expr(init),
                Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
            ) {
                self.seen_member_spans
                    .insert(unwrap_expr(init).span().start);
            }
        }
        walk::walk_variable_declarator(self, decl);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let _ = self.process_http_call(call);
        walk::walk_call_expression(self, call);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        match expr {
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                self.inspect_member_chain(expr);
                // Mark nested object spans in the same chain so `a.b.c` isn't also emitted as `a.b`.
                mark_inner_member_spans(expr, &mut self.seen_member_spans);
            }
            _ => {}
        }
        walk::walk_expression(self, expr);
    }
}

fn unwrap_expr<'a>(mut expr: &'a Expression<'a>) -> &'a Expression<'a> {
    loop {
        match expr {
            Expression::AwaitExpression(aw) => expr = &aw.argument,
            Expression::ParenthesizedExpression(p) => expr = &p.expression,
            Expression::TSAsExpression(ts) => expr = &ts.expression,
            Expression::TSSatisfiesExpression(ts) => expr = &ts.expression,
            Expression::TSTypeAssertion(ts) => expr = &ts.expression,
            Expression::TSNonNullExpression(ts) => expr = &ts.expression,
            _ => return expr,
        }
    }
}

fn extract_fetch_method(call: &CallExpression<'_>) -> Option<HttpMethod> {
    let second_arg = call.arguments.get(1)?;
    let Argument::ObjectExpression(obj) = second_arg else {
        return None;
    };

    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(kv) = prop else {
            continue;
        };
        if property_key_name(&kv.key).as_deref() == Some("method")
            && let Expression::StringLiteral(lit) = unwrap_expr(&kv.value)
        {
            return HttpMethod::parse(lit.value.as_str()?);
        }
    }
    None
}

fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(ident) => Some(ident.name.to_string()),
        PropertyKey::StringLiteral(lit) => lit.value.as_str().map(ToOwned::to_owned),
        _ => None,
    }
}

fn extract_member_chain(expr: &Expression<'_>) -> Option<(String, Vec<String>)> {
    let mut segments = Vec::new();
    let mut current = unwrap_expr(expr);

    loop {
        match current {
            Expression::StaticMemberExpression(member) => {
                segments.push(member.property.name.to_string());
                current = unwrap_expr(&member.object);
            }
            Expression::ChainExpression(chain) => match &chain.expression {
                ChainElement::StaticMemberExpression(member) => {
                    segments.push(member.property.name.to_string());
                    current = unwrap_expr(&member.object);
                }
                _ => return None,
            },
            Expression::Identifier(ident) => {
                if segments.is_empty() {
                    return None;
                }
                segments.reverse();
                return Some((ident.name.to_string(), segments));
            }
            _ => return None,
        }
    }
}

fn mark_inner_member_spans(expr: &Expression<'_>, seen: &mut HashSet<u32>) {
    let mut current = unwrap_expr(expr);
    loop {
        match current {
            Expression::StaticMemberExpression(member) => {
                seen.insert(member.span.start);
                current = unwrap_expr(&member.object);
            }
            Expression::ChainExpression(chain) => {
                seen.insert(chain.span.start);
                if let ChainElement::StaticMemberExpression(member) = &chain.expression {
                    seen.insert(member.span.start);
                    current = unwrap_expr(&member.object);
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
}

fn join_field_path(prefix: &str, segments: &[String]) -> String {
    if prefix.is_empty() {
        segments.join(".")
    } else if segments.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}.{}", segments.join("."))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use laine_core::config::WorkspaceConfig;
    use laine_core::fixtures::create_demo_workspace;
    use laine_core::git::GitRepo;

    use super::*;

    fn customer_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            name: "customer-service".into(),
            path: PathBuf::from("./customer-service"),
            openapi: Some(PathBuf::from("openapi.yaml")),
            base_urls: vec!["http://customer-service".into()],
        }]
    }

    #[test]
    fn extracts_destructuring_aliases_optional_chaining_and_const_base_url() {
        let code = r#"
import axios from "axios";

export async function loadProfile(id: string) {
  const BASE = "http://customer-service";
  const res = await fetch(`${BASE}/customers/${id}`);
  const customer = (await res.json()) as { fullName: string; email: string };
  const alias = customer;
  const { email: contactEmail } = alias;
  return `${alias?.fullName} <${contactEmail}>`;
}

export async function loadAxiosDestructured(id: string) {
  const { data } = await axios.get(`http://customer-service/customers/${id}`);
  return data.fullName;
}
"#;
        let extracted = extract_file("src/profile.ts", code, &customer_services())
            .expect("valid TS should extract");

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].client, HttpClientKind::Fetch);
        assert_eq!(extracted.calls[0].normalized_path, "/customers/:id");
        assert_eq!(extracted.calls[1].client, HttpClientKind::Axios);
        assert!(extracted.unresolved_calls.is_empty());

        let fields: Vec<(usize, &str)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str()))
            .collect();

        assert!(fields.contains(&(0, "email")));
        assert!(fields.contains(&(0, "fullName")));
        assert!(fields.contains(&(1, "fullName")));
    }

    #[test]
    fn extracts_all_consumer_repos_from_real_demo_fixture() {
        let tmp =
            std::env::temp_dir().join(format!("laine-ts-fixture-test-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
        let cfg = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();

        // 1. billing-service: fetch + fullName & email, plus unresolved dynamic URL in sync.ts
        let billing_repo = GitRepo::new(&paths.billing_service);
        let billing_code = billing_repo
            .read_file("main", Path::new("src/invoices/customer.ts"))
            .unwrap();
        let billing = extract_file("src/invoices/customer.ts", &billing_code, &cfg.services)
            .expect("billing customer.ts should extract");

        assert_eq!(billing.calls.len(), 1);
        assert_eq!(billing.calls[0].service, "customer-service");
        assert_eq!(billing.calls[0].method, HttpMethod::Get);
        assert_eq!(billing.calls[0].normalized_path, "/customers/:id");
        assert!(
            billing
                .field_reads
                .iter()
                .any(|r| r.field_path == "fullName")
        );
        assert!(billing.field_reads.iter().any(|r| r.field_path == "email"));

        let legacy_code = billing_repo
            .read_file("main", Path::new("src/legacy/sync.ts"))
            .unwrap();
        let legacy = extract_file("src/legacy/sync.ts", &legacy_code, &cfg.services)
            .expect("billing sync.ts should extract");
        assert!(legacy.calls.is_empty());
        assert_eq!(legacy.unresolved_calls.len(), 1);
        assert_eq!(legacy.unresolved_calls[0].client, HttpClientKind::Fetch);

        // 2. notification-service: axios.get + response.data.fullName
        let notif_repo = GitRepo::new(&paths.notification_service);
        let notif_code = notif_repo
            .read_file("main", Path::new("src/email/welcome.ts"))
            .unwrap();
        let notif = extract_file("src/email/welcome.ts", &notif_code, &cfg.services)
            .expect("notification welcome.ts should extract");

        assert_eq!(notif.calls.len(), 1);
        assert_eq!(notif.calls[0].client, HttpClientKind::Axios);
        assert_eq!(notif.calls[0].service, "customer-service");
        assert_eq!(notif.field_reads.len(), 1);
        assert_eq!(notif.field_reads[0].field_path, "fullName");

        // 3. shipping-service: fetch + customer.address ONLY (negative case)
        let ship_repo = GitRepo::new(&paths.shipping_service);
        let ship_code = ship_repo
            .read_file("main", Path::new("src/labels/address.ts"))
            .unwrap();
        let ship = extract_file("src/labels/address.ts", &ship_code, &cfg.services)
            .expect("shipping address.ts should extract");

        assert_eq!(ship.calls.len(), 1);
        assert_eq!(ship.field_reads.len(), 1);
        assert_eq!(ship.field_reads[0].field_path, "address");
        assert!(!ship.field_reads.iter().any(|r| r.field_path == "fullName"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
