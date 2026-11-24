use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use laine_core::config::{CustomClientConfig, ServiceConfig};
use laine_core::model::HttpMethod;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, ChainElement, Expression, Function,
    ObjectPropertyKind, Program, PropertyKey, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::url::{is_absolute_http_url, normalize_relative_path, resolve_url_against_services};
use crate::vue::extract_vue_scripts;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpClientKind {
    Fetch,
    Axios,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedHttpCall {
    pub id: usize,
    pub client: HttpClientKind,
    pub service: String,
    pub method: HttpMethod,
    pub normalized_path: String,
    pub response_type: Option<String>,
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

/// Workspace context passed to the TypeScript/JavaScript consumer extractor.
#[derive(Debug, Clone, Copy)]
pub struct ExtractContext<'a> {
    pub services: &'a [ServiceConfig],
    pub clients: &'a [CustomClientConfig],
    pub env: &'a BTreeMap<String, String>,
}

/// Convenience entrypoint using only configured `services` (empty custom clients & env maps).
pub fn extract_file(
    file_path: &str,
    source: &str,
    services: &[ServiceConfig],
) -> Result<FileExtraction, Vec<String>> {
    let empty_env = BTreeMap::new();
    extract_file_with_context(
        file_path,
        source,
        ExtractContext {
            services,
            clients: &[],
            env: &empty_env,
        },
    )
}

/// Extracts HTTP consumer calls (`fetch`, `axios`, `axios.create`, and configured custom wrappers),
/// field reads (direct, destructured, aliased, optional chaining, and 1-level local helpers),
/// TypeScript generic response types, and unresolved dynamic URLs (`UNKNOWN`).
pub fn extract_file_with_context(
    file_path: &str,
    source: &str,
    ctx: ExtractContext<'_>,
) -> Result<FileExtraction, Vec<String>> {
    if file_path.to_ascii_lowercase().ends_with(".vue") {
        let blocks = extract_vue_scripts(source);
        let mut merged = FileExtraction {
            file_path: file_path.to_string(),
            ..Default::default()
        };

        for block in blocks {
            let source_type = if block.is_ts {
                SourceType::ts()
            } else {
                SourceType::mjs()
            };
            let extracted = extract_source_block(file_path, &block.code, source_type, ctx)?;
            let call_offset = merged.calls.len();

            for mut call in extracted.calls {
                call.id += call_offset;
                merged.calls.push(call);
            }
            for mut read in extracted.field_reads {
                read.call_id += call_offset;
                merged.field_reads.push(read);
            }
            merged.unresolved_calls.extend(extracted.unresolved_calls);
        }

        return Ok(merged);
    }

    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::ts());
    extract_source_block(file_path, source, source_type, ctx)
}

fn extract_source_block(
    file_path: &str,
    source: &str,
    source_type: SourceType,
    ctx: ExtractContext<'_>,
) -> Result<FileExtraction, Vec<String>> {
    let allocator = Allocator::default();
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

    let helper_reads = collect_local_helper_reads(&parsed.program);
    let mut extractor = FileExtractor::new(file_path, source, ctx, helper_reads);
    extractor.visit_program(&parsed.program);
    Ok(extractor.finish())
}

#[derive(Debug, Clone)]
enum VarBinding {
    StringLiteral(String),
    HttpClientInstance { base_url: String },
    FetchResponse(usize),
    AxiosResponse(usize),
    JsonPayload { call_id: usize, prefix: String },
}

#[derive(Debug, Clone)]
enum UrlEval {
    Resolved(String),
    Dynamic(String),
}

#[derive(Debug, Clone)]
enum CallTarget {
    Fetch(HttpMethod),
    Axios(HttpMethod),
    AxiosInstance {
        base_url: String,
        method: HttpMethod,
    },
    Custom {
        target_service: String,
        path_prefix: Option<String>,
        method: HttpMethod,
    },
}

#[derive(Debug, Clone)]
struct HelperParamRead {
    param_index: usize,
    field_path: String,
    span: Span,
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

/// First pass: scans local helper functions (including exported functions & arrow functions)
/// to record which fields each parameter reads.
/// Enables 1-level local helper propagation (e.g. `formatName(customer)` -> reads `fullName`).
fn collect_local_helper_reads(program: &Program<'_>) -> HashMap<String, Vec<HelperParamRead>> {
    struct HelperCollector {
        helpers: HashMap<String, Vec<HelperParamRead>>,
    }

    impl<'a> Visit<'a> for HelperCollector {
        fn visit_function(&mut self, func: &Function<'a>, _flags: oxc_semantic::ScopeFlags) {
            if let Some(id) = &func.id {
                let reads = scan_function_param_reads(func);
                if !reads.is_empty() {
                    self.helpers.insert(id.name.to_string(), reads);
                }
            }
        }

        fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
            if let BindingPattern::BindingIdentifier(ident) = &decl.id
                && let Some(Expression::ArrowFunctionExpression(arrow)) = decl.init.as_ref()
            {
                let mut param_map = HashMap::new();
                let mut reads = Vec::new();
                for (idx, param) in arrow.params.items.iter().enumerate() {
                    register_param_pattern(&param.pattern, idx, "", &mut param_map, &mut reads);
                }
                let mut scanner = ParamReadScanner {
                    params: param_map,
                    reads,
                    seen_spans: HashSet::new(),
                };
                scanner.visit_arrow_function_body(&arrow.body);
                if !scanner.reads.is_empty() {
                    self.helpers.insert(ident.name.to_string(), scanner.reads);
                }
            }
        }
    }

    let mut collector = HelperCollector {
        helpers: HashMap::new(),
    };
    collector.visit_program(program);
    collector.helpers
}

fn scan_function_param_reads(func: &Function<'_>) -> Vec<HelperParamRead> {
    let Some(body) = &func.body else {
        return Vec::new();
    };
    let mut param_map = HashMap::new();
    let mut reads = Vec::new();
    for (idx, param) in func.params.items.iter().enumerate() {
        register_param_pattern(&param.pattern, idx, "", &mut param_map, &mut reads);
    }
    let mut scanner = ParamReadScanner {
        params: param_map,
        reads,
        seen_spans: HashSet::new(),
    };
    scanner.visit_function_body(body);
    scanner.reads
}

fn register_param_pattern(
    pattern: &BindingPattern<'_>,
    param_index: usize,
    prefix: &str,
    param_map: &mut HashMap<String, (usize, String)>,
    reads: &mut Vec<HelperParamRead>,
) {
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            param_map.insert(ident.name.to_string(), (param_index, prefix.to_string()));
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                if let Some(key) = property_key_name(&prop.key) {
                    let full = join_field_path(prefix, &[key]);
                    reads.push(HelperParamRead {
                        param_index,
                        field_path: full.clone(),
                        span: prop.span,
                    });
                    register_param_pattern(&prop.value, param_index, &full, param_map, reads);
                }
            }
        }
        BindingPattern::AssignmentPattern(assign) => {
            register_param_pattern(&assign.left, param_index, prefix, param_map, reads);
        }
        BindingPattern::ArrayPattern(_) => {}
    }
}

struct ParamReadScanner {
    params: HashMap<String, (usize, String)>,
    reads: Vec<HelperParamRead>,
    seen_spans: HashSet<u32>,
}

impl<'a> Visit<'a> for ParamReadScanner {
    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if matches!(
            expr,
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
        ) && self.seen_spans.insert(expr.span().start)
        {
            if let Some((root, segments)) = extract_member_chain(expr)
                && let Some((param_index, prefix)) = self.params.get(&root)
            {
                let field_path = join_field_path(prefix, &segments);
                self.reads.push(HelperParamRead {
                    param_index: *param_index,
                    field_path,
                    span: expr.span(),
                });
            }
            mark_inner_member_spans(expr, &mut self.seen_spans);
        }
        walk::walk_expression(self, expr);
    }
}

struct FileExtractor<'s> {
    file_path: String,
    source: &'s str,
    line_index: LineIndex,
    ctx: ExtractContext<'s>,
    helper_reads: HashMap<String, Vec<HelperParamRead>>,
    scopes: Vec<HashMap<String, VarBinding>>,
    calls: Vec<ExtractedHttpCall>,
    field_reads: Vec<ExtractedFieldRead>,
    unresolved_calls: Vec<ExtractedUnresolvedCall>,
    seen_call_spans: HashSet<u32>,
    seen_member_spans: HashSet<u32>,
    seen_reads: HashSet<(usize, String, usize)>,
}

impl<'s> FileExtractor<'s> {
    fn new(
        file_path: &str,
        source: &'s str,
        ctx: ExtractContext<'s>,
        helper_reads: HashMap<String, Vec<HelperParamRead>>,
    ) -> Self {
        Self {
            file_path: file_path.to_string(),
            source,
            line_index: LineIndex::new(source),
            ctx,
            helper_reads,
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

    fn extract_response_type(&self, call: &CallExpression<'_>) -> Option<String> {
        let type_args = call.type_arguments.as_ref()?;
        let first = type_args.params.first()?;
        let span = first.span();
        let raw = self.source.get(span.start as usize..span.end as usize)?;
        let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    fn detect_axios_create(&self, call: &CallExpression<'_>) -> Option<VarBinding> {
        let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) else {
            return None;
        };
        let Expression::Identifier(obj) = unwrap_expr(&member.object) else {
            return None;
        };
        if obj.name != "axios" || member.property.name != "create" {
            return None;
        }
        let Argument::ObjectExpression(opts) = call.arguments.first()? else {
            return None;
        };

        for prop in &opts.properties {
            let ObjectPropertyKind::ObjectProperty(kv) = prop else {
                continue;
            };
            if property_key_name(&kv.key).as_deref() == Some("baseURL")
                && let UrlEval::Resolved(base_url) = self.eval_url_expr(&kv.value)
            {
                return Some(VarBinding::HttpClientInstance { base_url });
            }
        }
        None
    }

    fn classify_http_call(&self, call: &CallExpression<'_>) -> Option<CallTarget> {
        // 1. Check configured custom clients first (so `$fetch`, `getClient().fetch`, `apiClient.post` work).
        if let Some(callee_str) = format_callee_expr(unwrap_expr(&call.callee)) {
            for client in self.ctx.clients {
                if callee_str == client.callee {
                    let method = extract_options_method(call).unwrap_or(HttpMethod::Get);
                    return Some(CallTarget::Custom {
                        target_service: client.target_service.clone(),
                        path_prefix: client.path_prefix.clone(),
                        method,
                    });
                }

                if let Some(rest) = callee_str.strip_prefix(&client.callee)
                    && let Some(method_name) = rest.strip_prefix('.')
                {
                    if method_name == "fetch" {
                        let method = extract_options_method(call).unwrap_or(HttpMethod::Get);
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method,
                        });
                    }
                    if let Some(method) = HttpMethod::parse(method_name) {
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method,
                        });
                    }
                }
            }
        }

        // 2. Built-in `fetch`, `axios.<method>`, and local `axios.create(...)` instances.
        match unwrap_expr(&call.callee) {
            Expression::Identifier(ident) if ident.name == "fetch" => {
                let method = extract_options_method(call).unwrap_or(HttpMethod::Get);
                Some(CallTarget::Fetch(method))
            }
            Expression::StaticMemberExpression(member) => {
                let Expression::Identifier(obj) = unwrap_expr(&member.object) else {
                    return None;
                };
                let method = HttpMethod::parse(member.property.name.as_str())?;
                if obj.name == "axios" {
                    return Some(CallTarget::Axios(method));
                }
                if let Some(VarBinding::HttpClientInstance { base_url }) =
                    self.lookup_var(obj.name.as_str())
                {
                    return Some(CallTarget::AxiosInstance {
                        base_url: base_url.clone(),
                        method,
                    });
                }
                None
            }
            _ => None,
        }
    }

    fn process_http_call(&mut self, call: &CallExpression<'_>) -> Option<(HttpClientKind, usize)> {
        let target = self.classify_http_call(call)?;
        if !self.seen_call_spans.insert(call.span.start) {
            return None;
        }

        let (client_kind, method) = match &target {
            CallTarget::Fetch(m) => (HttpClientKind::Fetch, *m),
            CallTarget::Axios(m) | CallTarget::AxiosInstance { method: m, .. } => {
                (HttpClientKind::Axios, *m)
            }
            CallTarget::Custom { method: m, .. } => (HttpClientKind::Custom, *m),
        };

        let (line, snippet) = self.location(call.span);
        let response_type = self.extract_response_type(call);

        let Some(first_arg) = call.arguments.first() else {
            self.unresolved_calls.push(ExtractedUnresolvedCall {
                client: client_kind,
                line,
                snippet,
                reason: "missing URL argument".to_string(),
            });
            return None;
        };

        let Some(url_expr) = first_arg.as_expression() else {
            self.unresolved_calls.push(ExtractedUnresolvedCall {
                client: client_kind,
                line,
                snippet,
                reason: "spread argument cannot be resolved statically".to_string(),
            });
            return None;
        };

        match self.eval_url_expr(url_expr) {
            UrlEval::Resolved(url_str) => {
                let matched = match &target {
                    CallTarget::Fetch(_) | CallTarget::Axios(_) => {
                        resolve_url_against_services(&url_str, self.ctx.services)
                    }
                    CallTarget::AxiosInstance { base_url, .. } => {
                        let full_url = if is_absolute_http_url(&url_str) {
                            url_str
                        } else {
                            let base = base_url.trim_end_matches('/');
                            let rel = url_str.trim_start_matches('/');
                            format!("{base}/{rel}")
                        };
                        resolve_url_against_services(&full_url, self.ctx.services)
                    }
                    CallTarget::Custom {
                        target_service,
                        path_prefix,
                        ..
                    } => {
                        if is_absolute_http_url(&url_str) {
                            resolve_url_against_services(&url_str, self.ctx.services)
                        } else {
                            Some(crate::url::ServiceEndpointMatch {
                                service: target_service.clone(),
                                normalized_path: normalize_relative_path(
                                    &url_str,
                                    path_prefix.as_deref(),
                                ),
                            })
                        }
                    }
                };

                if let Some(matched) = matched {
                    let id = self.calls.len();
                    self.calls.push(ExtractedHttpCall {
                        id,
                        client: client_kind,
                        service: matched.service,
                        method,
                        normalized_path: matched.normalized_path,
                        response_type,
                        line,
                        snippet,
                    });
                    Some((client_kind, id))
                } else {
                    None
                }
            }
            UrlEval::Dynamic(reason) => {
                self.unresolved_calls.push(ExtractedUnresolvedCall {
                    client: client_kind,
                    line,
                    snippet,
                    reason,
                });
                None
            }
        }
    }

    fn process_helper_call(&mut self, call: &CallExpression<'_>) {
        let Expression::Identifier(callee) = unwrap_expr(&call.callee) else {
            return;
        };
        let Some(helper_reads) = self.helper_reads.get(callee.name.as_str()).cloned() else {
            return;
        };

        for (arg_idx, arg) in call.arguments.iter().enumerate() {
            let Some(arg_expr) = arg.as_expression() else {
                continue;
            };
            let Some(VarBinding::JsonPayload { call_id, prefix }) =
                self.resolve_binding_from_expr(arg_expr)
            else {
                continue;
            };

            for hr in helper_reads.iter().filter(|r| r.param_index == arg_idx) {
                let full_path = join_field_path(&prefix, std::slice::from_ref(&hr.field_path));
                self.record_field_read(call_id, full_path, hr.span);
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
            Expression::StaticMemberExpression(_) => {
                if let Some(env_name) = extract_env_var_name(unwrap_expr(expr)) {
                    if let Some(val) = self.ctx.env.get(&env_name) {
                        return UrlEval::Resolved(val.clone());
                    }
                    return UrlEval::Dynamic(format!(
                        "unmapped environment variable `{env_name}` (configure under `[env]` in laine.toml)"
                    ));
                }
                UrlEval::Dynamic("dynamic URL expression cannot be resolved statically".into())
            }
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
                let unwrapped = unwrap_expr(expr);
                match unwrapped {
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
                    Expression::StaticMemberExpression(_) => {
                        if let Some(env_name) = extract_env_var_name(unwrapped) {
                            if let Some(val) = self.ctx.env.get(&env_name) {
                                out.push_str(val);
                            } else if out.is_empty() {
                                return UrlEval::Dynamic(format!(
                                    "unmapped environment variable `{env_name}`"
                                ));
                            } else {
                                out.push_str(":param");
                            }
                        } else if out.is_empty() {
                            return UrlEval::Dynamic(
                                "dynamic template URL prefix cannot be resolved statically".into(),
                            );
                        } else {
                            out.push_str(":param");
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
            Expression::StaticMemberExpression(_) if extract_env_var_name(unwrapped).is_some() => {
                let env_name = extract_env_var_name(unwrapped)?;
                self.ctx
                    .env
                    .get(&env_name)
                    .cloned()
                    .map(VarBinding::StringLiteral)
            }
            Expression::CallExpression(call) => {
                // Local `axios.create({ baseURL })`
                if let Some(instance) = self.detect_axios_create(call) {
                    return Some(instance);
                }

                // Direct `fetch(...)`, `axios.get(...)`, or configured custom client
                if let Some((client, call_id)) = self.process_http_call(call) {
                    return Some(match client {
                        HttpClientKind::Fetch => VarBinding::FetchResponse(call_id),
                        HttpClientKind::Axios => VarBinding::AxiosResponse(call_id),
                        // Custom clients like `$fetch<T>` / `getClient().fetch<T>` return parsed JSON directly.
                        HttpClientKind::Custom => VarBinding::JsonPayload {
                            call_id,
                            prefix: String::new(),
                        },
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

    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_semantic::ScopeFlags) {
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
        self.process_helper_call(call);
        walk::walk_call_expression(self, call);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        match expr {
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                self.inspect_member_chain(expr);
                mark_inner_member_spans(expr, &mut self.seen_member_spans);
            }
            _ => {}
        }
        walk::walk_expression(self, expr);
    }
}

fn format_callee_expr(expr: &Expression<'_>) -> Option<String> {
    match unwrap_expr(expr) {
        Expression::Identifier(ident) => Some(ident.name.to_string()),
        Expression::StaticMemberExpression(member) => {
            let left = format_callee_expr(&member.object)?;
            Some(format!("{left}.{}", member.property.name))
        }
        Expression::CallExpression(call) if call.arguments.is_empty() => {
            let inner = format_callee_expr(&call.callee)?;
            Some(format!("{inner}()"))
        }
        _ => None,
    }
}

fn extract_env_var_name(expr: &Expression<'_>) -> Option<String> {
    let Expression::StaticMemberExpression(outer) = unwrap_expr(expr) else {
        return None;
    };
    let env_name = outer.property.name.to_string();
    let Expression::StaticMemberExpression(middle) = unwrap_expr(&outer.object) else {
        return None;
    };
    if middle.property.name != "env" {
        return None;
    }

    match unwrap_expr(&middle.object) {
        Expression::Identifier(id) if id.name == "process" => Some(env_name),
        Expression::ImportMeta(_) => Some(env_name),
        _ => None,
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

fn extract_options_method(call: &CallExpression<'_>) -> Option<HttpMethod> {
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
            routes: None,
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
  const { data } = await axios.get<CustomerDTO>(`http://customer-service/customers/${id}`);
  return data.fullName;
}
"#;
        let extracted = extract_file("src/profile.ts", code, &customer_services())
            .expect("valid TS should extract");

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].client, HttpClientKind::Fetch);
        assert_eq!(extracted.calls[0].normalized_path, "/customers/:id");
        assert_eq!(extracted.calls[1].client, HttpClientKind::Axios);
        assert_eq!(
            extracted.calls[1].response_type.as_deref(),
            Some("CustomerDTO")
        );
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
    fn extracts_custom_clients_axios_create_env_vars_and_local_helpers() {
        let services = customer_services();
        let clients = vec![
            CustomClientConfig {
                callee: "getClient().fetch".into(),
                target_service: "customer-service".into(),
                path_prefix: Some("/api".into()),
            },
            CustomClientConfig {
                callee: "apiClient".into(),
                target_service: "customer-service".into(),
                path_prefix: None,
            },
        ];
        let mut env = BTreeMap::new();
        env.insert(
            "VITE_CUSTOMER_API".to_string(),
            "http://customer-service".to_string(),
        );

        let code = r#"
import axios from "axios";

function formatCustomerLabel(c: { fullName: string }) {
  return `Customer: ${c.fullName}`;
}

export const service = {
  async listBookings(payload: unknown) {
    const res = await getClient().fetch<BookingListResponse>('/corp/listBooking', {
      method: 'POST',
      body: payload,
    });
    return res.items;
  },

  async getByAxiosInstance(id: string) {
    const http = axios.create({ baseURL: import.meta.env.VITE_CUSTOMER_API });
    const resp = await http.get(`/customers/${id}`);
    const customer = resp.data;
    return formatCustomerLabel(customer);
  },

  async deleteCustomer(id: string) {
    return apiClient.delete(`/customers/${id}`);
  },

  async unmappedEnv(id: string) {
    return fetch(`${process.env.UNKNOWN_API}/customers/${id}`);
  },
};
"#;

        let extracted = extract_file_with_context(
            "src/service.ts",
            code,
            ExtractContext {
                services: &services,
                clients: &clients,
                env: &env,
            },
        )
        .expect("custom clients TS should extract");

        assert_eq!(extracted.calls.len(), 3);

        // 1. Custom wrapper `getClient().fetch<BookingListResponse>('/corp/listBooking', { method: 'POST' })`
        assert_eq!(extracted.calls[0].client, HttpClientKind::Custom);
        assert_eq!(extracted.calls[0].method, HttpMethod::Post);
        assert_eq!(extracted.calls[0].normalized_path, "/api/corp/listBooking");
        assert_eq!(
            extracted.calls[0].response_type.as_deref(),
            Some("BookingListResponse")
        );

        // 2. `axios.create({ baseURL: import.meta.env.VITE_CUSTOMER_API })` + helper `formatCustomerLabel`
        assert_eq!(extracted.calls[1].client, HttpClientKind::Axios);
        assert_eq!(extracted.calls[1].service, "customer-service");
        assert_eq!(extracted.calls[1].normalized_path, "/customers/:id");

        // 3. `apiClient.delete(...)`
        assert_eq!(extracted.calls[2].client, HttpClientKind::Custom);
        assert_eq!(extracted.calls[2].method, HttpMethod::Delete);
        assert_eq!(extracted.calls[2].normalized_path, "/customers/:id");

        // Field reads include direct `res.items` AND helper-propagated `c.fullName`!
        let fields: Vec<(usize, &str)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str()))
            .collect();
        assert!(fields.contains(&(0, "items")));
        assert!(fields.contains(&(1, "fullName")));

        // Unmapped env var reported as UNKNOWN!
        assert_eq!(extracted.unresolved_calls.len(), 1);
        assert!(extracted.unresolved_calls[0].reason.contains("UNKNOWN_API"));
    }

    #[test]
    fn extracts_all_consumer_repos_from_real_demo_fixture() {
        let tmp =
            std::env::temp_dir().join(format!("laine-ts-fixture-test-{}", std::process::id()));
        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
        let cfg = WorkspaceConfig::load_from_path(&paths.config_path).unwrap();

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
