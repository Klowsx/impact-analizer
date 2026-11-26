use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use laine_core::config::{CustomClientConfig, ServiceConfig, resolve_http_service_url};
use laine_core::model::HttpMethod;
use laine_core::request::{
    Argument as RequestArgument, ArgumentType, HttpRequestEvidence, RequestSection,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, AssignmentExpression, AssignmentOperator, AssignmentTarget, BindingPattern,
    CallExpression, ChainElement, Expression, FormalParameters, Function, ObjectExpression,
    ObjectPropertyKind, Program, PropertyKey, Statement, TSTypeAnnotation, TemplateLiteral,
    VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::cross_file::FileAxiosImports;
use crate::url::{is_absolute_http_url, normalize_relative_path};
use crate::vue::{extract_vue_scripts_with_setup, extract_vue_template_expressions};

const UNMAPPED_ENV: &str = "unmapped environment variable (configure under `[env]` in laine.toml)";

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
    #[serde(default)]
    pub response_type_is_schema: bool,
    #[serde(default)]
    pub request: HttpRequestEvidence,
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
// Extrae llamadas con el lenguaje original de cada bloque Vue.
pub fn extract_file_with_context(
    file_path: &str,
    source: &str,
    ctx: ExtractContext<'_>,
) -> Result<FileExtraction, Vec<String>> {
    extract_file_with_imported_axios(file_path, source, ctx, None)
}

/// Extracts calls and response reads with the Axios bindings resolved for this module.
pub fn extract_file_with_imported_axios(
    file_path: &str,
    source: &str,
    ctx: ExtractContext<'_>,
    axios_imports: Option<&FileAxiosImports>,
) -> Result<FileExtraction, Vec<String>> {
    if file_path.to_ascii_lowercase().ends_with(".vue") {
        let blocks = extract_vue_scripts_with_setup(source);
        let mut merged = FileExtraction {
            file_path: file_path.to_string(),
            ..Default::default()
        };
        let mut template_bindings: HashMap<String, VarBinding> = HashMap::new();

        for (block, is_setup) in blocks {
            let source_type = block.lang.source_type();
            let (extracted, top_bindings) = extract_source_block_with_bindings(
                file_path,
                &block.code,
                source_type,
                ctx,
                axios_imports,
            )?;
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

            if is_setup {
                for (name, binding) in top_bindings {
                    if binding.is_template_trackable() {
                        template_bindings.insert(name, binding.with_call_offset(call_offset));
                    }
                }
            }
        }

        if !template_bindings.is_empty() {
            let mut seen_reads: HashSet<(usize, String, usize)> = merged
                .field_reads
                .iter()
                .map(|r| (r.call_id, r.field_path.clone(), r.line))
                .collect();
            scan_vue_template_reads(
                source,
                &template_bindings,
                &mut seen_reads,
                &mut merged.field_reads,
            );
        }

        return Ok(merged);
    }

    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::ts());
    let (extracted, _) =
        extract_source_block_with_bindings(file_path, source, source_type, ctx, axios_imports)?;
    Ok(extracted)
}

/// Parses a script and makes its imported instances available to the call visitor.
fn extract_source_block_with_bindings(
    file_path: &str,
    source: &str,
    source_type: SourceType,
    ctx: ExtractContext<'_>,
    axios_imports: Option<&FileAxiosImports>,
) -> Result<(FileExtraction, HashMap<String, VarBinding>), Vec<String>> {
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
    if let Some(imports) = axios_imports {
        for (name, base_url) in imports.bindings(&parsed.program) {
            extractor.bind_var(name, VarBinding::HttpClientInstance { base_url });
        }
    }
    extractor.visit_program(&parsed.program);
    Ok(extractor.finish_with_top_bindings())
}

fn scan_vue_template_reads(
    source: &str,
    bindings: &HashMap<String, VarBinding>,
    seen_reads: &mut HashSet<(usize, String, usize)>,
    field_reads: &mut Vec<ExtractedFieldRead>,
) {
    let template_exprs = extract_vue_template_expressions(source);
    for tpl_expr in template_exprs {
        let allocator = Allocator::default();
        let Ok(parsed_expr) =
            Parser::new(&allocator, &tpl_expr.expr, SourceType::ts()).parse_expression()
        else {
            continue;
        };

        let mut scanner = TemplateExprScanner {
            bindings,
            line: tpl_expr.line,
            snippet: &tpl_expr.snippet,
            seen_member_spans: HashSet::new(),
            seen_reads,
            field_reads,
        };
        scanner.visit_expression(&parsed_expr);
    }
}

struct TemplateExprScanner<'a> {
    bindings: &'a HashMap<String, VarBinding>,
    line: usize,
    snippet: &'a str,
    seen_member_spans: HashSet<u32>,
    seen_reads: &'a mut HashSet<(usize, String, usize)>,
    field_reads: &'a mut Vec<ExtractedFieldRead>,
}

impl TemplateExprScanner<'_> {
    fn inspect_member_chain(&mut self, expr: &Expression<'_>) {
        let span = expr.span();
        if self.seen_member_spans.contains(&span.start) {
            return;
        }
        let Some((root_name, segments)) = extract_member_chain(expr) else {
            return;
        };
        self.seen_member_spans.insert(span.start);
        mark_inner_member_spans(expr, &mut self.seen_member_spans);

        let Some(root_binding) = self.bindings.get(&root_name) else {
            return;
        };

        match root_binding {
            VarBinding::JsonPayload {
                call_id, prefix, ..
            } => {
                let full_path = join_field_path(prefix, &segments);
                self.record_field_read(*call_id, full_path);
            }
            VarBinding::AxiosResponse { call_id, .. } | VarBinding::UseFetchResponse(call_id)
                if segments.first().map(String::as_str) == Some("data") && segments.len() > 1 =>
            {
                let full_path = segments[1..].join(".");
                self.record_field_read(*call_id, full_path);
            }
            _ => {}
        }
    }

    fn record_field_read(&mut self, call_id: usize, field_path: String) {
        if field_path.is_empty() {
            return;
        }
        if self
            .seen_reads
            .insert((call_id, field_path.clone(), self.line))
        {
            self.field_reads.push(ExtractedFieldRead {
                call_id,
                field_path,
                line: self.line,
                snippet: self.snippet.to_string(),
            });
        }
    }
}

impl<'a> Visit<'a> for TemplateExprScanner<'_> {
    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if matches!(
            expr,
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
        ) {
            self.inspect_member_chain(expr);
        }
        walk::walk_expression(self, expr);
    }
}

#[derive(Debug, Clone)]
enum VarBinding {
    Unassigned,
    VueRefEmpty {
        declared_type: Option<String>,
    },
    StringLiteral(String),
    HttpClientInstance {
        base_url: Option<String>,
    },
    FetchResponse {
        call_id: usize,
        is_vue_ref: bool,
    },
    AxiosResponse {
        call_id: usize,
        is_vue_ref: bool,
    },
    UseFetchResponse(usize),
    JsonPayload {
        call_id: usize,
        prefix: String,
        is_vue_ref: bool,
    },
}

impl VarBinding {
    fn is_template_trackable(&self) -> bool {
        matches!(
            self,
            Self::JsonPayload { .. } | Self::AxiosResponse { .. } | Self::UseFetchResponse(_)
        )
    }

    fn with_call_offset(self, offset: usize) -> Self {
        match self {
            Self::FetchResponse {
                call_id,
                is_vue_ref,
            } => Self::FetchResponse {
                call_id: call_id + offset,
                is_vue_ref,
            },
            Self::AxiosResponse {
                call_id,
                is_vue_ref,
            } => Self::AxiosResponse {
                call_id: call_id + offset,
                is_vue_ref,
            },
            Self::UseFetchResponse(call_id) => Self::UseFetchResponse(call_id + offset),
            Self::JsonPayload {
                call_id,
                prefix,
                is_vue_ref,
            } => Self::JsonPayload {
                call_id: call_id + offset,
                prefix,
                is_vue_ref,
            },
            other => other,
        }
    }
}

#[derive(Debug, Clone)]
enum UrlEval {
    Resolved(String),
    Dynamic(String),
}

#[derive(Debug, Clone)]
enum CallTarget {
    Fetch(Option<HttpMethod>),
    UseFetch(Option<HttpMethod>),
    Axios(HttpMethod),
    AxiosInstance {
        base_url: Option<String>,
        method: HttpMethod,
    },
    Custom {
        target_service: String,
        path_prefix: Option<String>,
        method: Option<HttpMethod>,
        is_use_fetch: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum CallResultShape {
    FetchResponse,
    AxiosResponse,
    UseFetchResponse,
    JsonPayload,
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
    return_type_stack: Vec<Option<String>>,
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
            return_type_stack: Vec::new(),
            calls: Vec::new(),
            field_reads: Vec::new(),
            unresolved_calls: Vec::new(),
            seen_call_spans: HashSet::new(),
            seen_member_spans: HashSet::new(),
            seen_reads: HashSet::new(),
        }
    }

    fn finish_with_top_bindings(mut self) -> (FileExtraction, HashMap<String, VarBinding>) {
        let top_bindings = self.scopes.drain(..).next().unwrap_or_default();
        let extraction = FileExtraction {
            file_path: self.file_path,
            calls: self.calls,
            field_reads: self.field_reads,
            unresolved_calls: self.unresolved_calls,
        };
        (extraction, top_bindings)
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

    fn update_or_bind_var(&mut self, name: &str, binding: VarBinding) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = binding;
                return;
            }
        }
        self.bind_var(name, binding);
    }

    fn lookup_var(&self, name: &str) -> Option<&VarBinding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn current_return_type(&self) -> Option<String> {
        self.return_type_stack.last().and_then(Clone::clone)
    }

    fn extract_fn_return_type(&self, return_type: Option<&TSTypeAnnotation<'_>>) -> Option<String> {
        let ann = return_type?;
        let span = ann.type_annotation.span();
        let raw = self.source.get(span.start as usize..span.end as usize)?;
        let normalized = normalize_type_expr(raw);
        let unwrapped = strip_outer_promise(&normalized);
        if is_non_dto_return_type(unwrapped) {
            None
        } else {
            Some(unwrapped.to_string())
        }
    }

    fn extract_vue_ref_type(&self, call: &CallExpression<'_>) -> Option<String> {
        let raw = self.extract_response_type(call)?;
        let unwrapped = strip_outer_promise(&raw);
        if is_non_dto_return_type(unwrapped) {
            None
        } else {
            Some(unwrapped.to_string())
        }
    }

    fn maybe_backfill_call_response_type(&mut self, call_id: usize, declared_type: Option<String>) {
        let Some(ty) = declared_type else {
            return;
        };
        if let Some(call) = self.calls.get_mut(call_id)
            && call.response_type.is_none()
        {
            call.response_type = Some(ty);
        }
    }

    /// Associates a runtime validator with the response when no explicit type is available.
    fn maybe_backfill_call_schema(&mut self, call_id: usize, schema: String) {
        if let Some(call) = self.calls.get_mut(call_id)
            && call.response_type.is_none()
        {
            call.response_type = Some(schema);
            call.response_type_is_schema = true;
        }
    }

    fn apply_ref_assignment_binding(
        &mut self,
        target_name: &str,
        is_vue_ref_assign: bool,
        binding: VarBinding,
    ) -> VarBinding {
        if let Some(VarBinding::VueRefEmpty {
            declared_type: Some(ty),
        }) = self.lookup_var(target_name).cloned()
            && let VarBinding::JsonPayload {
                call_id,
                ref prefix,
                ..
            } = binding
            && prefix.is_empty()
        {
            self.maybe_backfill_call_response_type(call_id, Some(ty));
        }

        if is_vue_ref_assign {
            match binding {
                VarBinding::JsonPayload {
                    call_id, prefix, ..
                } => VarBinding::JsonPayload {
                    call_id,
                    prefix,
                    is_vue_ref: true,
                },
                VarBinding::AxiosResponse { call_id, .. } => VarBinding::AxiosResponse {
                    call_id,
                    is_vue_ref: true,
                },
                VarBinding::FetchResponse { call_id, .. } => VarBinding::FetchResponse {
                    call_id,
                    is_vue_ref: true,
                },
                other => other,
            }
        } else {
            binding
        }
    }

    fn register_unassigned_pattern(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), VarBinding::Unassigned);
            }
            BindingPattern::ObjectPattern(obj_pat) => {
                for prop in &obj_pat.properties {
                    self.register_unassigned_pattern(&prop.value);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.register_unassigned_pattern(&assign.left);
            }
            BindingPattern::ArrayPattern(arr_pat) => {
                for elem in arr_pat.elements.iter().flatten() {
                    self.register_unassigned_pattern(elem);
                }
            }
        }
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
        let normalized = normalize_type_expr(raw);
        if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        }
    }

    /// Evaluates the base URL of a locally constructed Axios instance.
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
                return Some(VarBinding::HttpClientInstance {
                    base_url: Some(base_url),
                });
            }
        }
        None
    }

    // Recognizes configured clients, built-in clients, and scoped Axios instances.
    fn classify_http_call(&self, call: &CallExpression<'_>) -> Option<CallTarget> {
        // 1. Check configured custom clients first (so `$fetch`, `getClient().fetch`, `apiClient.post` work).
        if let Some(callee_str) = format_callee_expr(unwrap_expr(&call.callee)) {
            for client in self.ctx.clients {
                if callee_str == client.callee {
                    let method = options_http_method(call);
                    return Some(CallTarget::Custom {
                        target_service: client.target_service.clone(),
                        path_prefix: client.path_prefix.clone(),
                        method,
                        is_use_fetch: is_use_fetch_callee(&callee_str),
                    });
                }

                if let Some(rest) = callee_str.strip_prefix(&client.callee)
                    && let Some(method_name) = rest.strip_prefix('.')
                {
                    if method_name == "fetch" || is_use_fetch_callee(method_name) {
                        let method = options_http_method(call);
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method,
                            is_use_fetch: is_use_fetch_callee(method_name),
                        });
                    }
                    if let Some(method) = HttpMethod::parse(method_name) {
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method: Some(method),
                            is_use_fetch: false,
                        });
                    }
                }
            }
        }

        // 2. Built-in `fetch`, `useFetch` / `useLazyFetch`, `axios.<method>`, and local `axios.create(...)` instances.
        match unwrap_expr(&call.callee) {
            Expression::Identifier(ident) if ident.name == "fetch" => {
                let method = options_http_method(call);
                Some(CallTarget::Fetch(method))
            }
            Expression::Identifier(ident)
                if ident.name == "useFetch" || ident.name == "useLazyFetch" =>
            {
                let method = options_http_method(call);
                Some(CallTarget::UseFetch(method))
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

    // Extract arguments from built-in HTTP signatures without saving their values.
    fn request_evidence(
        &self,
        call: &CallExpression<'_>,
        target: &CallTarget,
        method: HttpMethod,
        url: &str,
        line: usize,
    ) -> HttpRequestEvidence {
        let mut request = match target {
            CallTarget::Fetch(_) | CallTarget::UseFetch(_) => {
                let nuxt = matches!(target, CallTarget::UseFetch(_));
                let body = match static_option(call, 1, "body") {
                    Some(Some(value)) if !nuxt => json_stringify_input(value).map(Some),
                    body => body,
                };
                HttpRequestEvidence {
                    body: self.request_section(body, false, false),
                    query: if nuxt {
                        let mut query =
                            self.request_section(static_option(call, 1, "query"), true, false);
                        if query.complete {
                            let params =
                                self.request_section(static_option(call, 1, "params"), true, false);
                            query.complete = params.complete;
                            for (name, argument) in params.fields {
                                query.fields.entry(name).or_insert(argument);
                            }
                        }
                        query
                    } else {
                        RequestSection::absent()
                    },
                    headers: self.request_section(static_option(call, 1, "headers"), true, true),
                }
            }
            CallTarget::Axios(_) | CallTarget::AxiosInstance { .. } => {
                let data_verb = matches!(
                    method,
                    HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch
                );
                let index = if data_verb { 2 } else { 1 };
                let body = if data_verb {
                    call_argument(call, 1)
                } else {
                    static_option(call, index, "data")
                };
                let mut headers =
                    self.request_section(static_option(call, index, "headers"), true, true);
                headers.complete = false;
                let mut query =
                    if matches!(static_option(call, index, "paramsSerializer"), Some(None)) {
                        self.request_section(static_option(call, index, "params"), true, false)
                    } else {
                        RequestSection::default()
                    };
                if matches!(target, CallTarget::AxiosInstance { .. }) {
                    query.complete = false;
                }
                HttpRequestEvidence {
                    body: if matches!(static_option(call, index, "transformRequest"), Some(None)) {
                        self.request_section(body, false, false)
                    } else {
                        RequestSection::default()
                    },
                    query,
                    headers,
                }
            }
            CallTarget::Custom { .. } => return HttpRequestEvidence::default(),
        };
        request.query.add_url_query(url, line);
        request
    }

    // Classify the argument and inspect properties of object literals.
    fn request_section(
        &self,
        value: Option<Option<&Expression<'_>>>,
        wire: bool,
        lowercase: bool,
    ) -> RequestSection {
        let Some(value) = value else {
            return RequestSection::default();
        };
        let Some(value) = value else {
            return RequestSection::absent();
        };
        let mut section = RequestSection::absent();
        section.root = Some(self.argument_type(value, wire));
        if let Expression::ObjectExpression(object) = unwrap_expr(value) {
            self.request_fields(object, "", wire, lowercase, &mut section);
        } else if wire
            || section
                .root
                .as_ref()
                .is_some_and(|arg| arg.types.is_empty())
        {
            section.complete = false;
        }
        section
    }

    // Keep static keys and discard keys an opaque property could overwrite.
    fn request_fields(
        &self,
        object: &ObjectExpression<'_>,
        prefix: &str,
        wire: bool,
        lowercase: bool,
        section: &mut RequestSection,
    ) {
        for property in &object.properties {
            let named = match property {
                ObjectPropertyKind::ObjectProperty(property) => property_key_name(&property.key)
                    .filter(|name| wire || !name.contains(['.', '[', ']']))
                    .map(|name| (name, &property.value)),
                ObjectPropertyKind::SpreadProperty(_) => None,
            };
            let Some((mut name, value)) = named else {
                section.complete = false;
                if prefix.is_empty() {
                    section.fields.clear();
                } else {
                    let children = format!("{prefix}.");
                    section
                        .fields
                        .retain(|path, _| !path.starts_with(&children));
                }
                continue;
            };
            if lowercase {
                name.make_ascii_lowercase();
            }
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}.{name}")
            };
            let children = format!("{path}.");
            section
                .fields
                .retain(|field, _| !field.starts_with(&children));
            let mut argument = self.argument_type(value, wire);
            // Clients can combine headers whose original spelling differs only by case.
            if lowercase && section.fields.contains_key(&path) {
                argument.types.clear();
            }
            section.fields.insert(path.clone(), argument);
            if !wire && let Expression::ObjectExpression(child) = unwrap_expr(value) {
                self.request_fields(child, &path, wire, lowercase, section);
            }
        }
    }

    // Classify JSON and wire literal types without retaining their values.
    fn argument_type(&self, value: &Expression<'_>, wire: bool) -> RequestArgument {
        let expr = unwrap_expr(value);
        let line = self.line_index.line_number(expr.span().start);
        let kind = match expr {
            Expression::NullLiteral(_) if !wire => Some(ArgumentType::Null),
            Expression::BooleanLiteral(literal) if wire => {
                return RequestArgument::wire_text(
                    line,
                    if literal.value { "true" } else { "false" },
                );
            }
            Expression::BooleanLiteral(_) => Some(ArgumentType::Boolean),
            Expression::NumericLiteral(literal) if wire => {
                return RequestArgument::wire_text(line, &literal.value.to_string());
            }
            Expression::NumericLiteral(literal) if literal.value.fract() == 0.0 => {
                Some(ArgumentType::Integer)
            }
            Expression::NumericLiteral(literal) if literal.value.is_finite() => {
                Some(ArgumentType::Number)
            }
            Expression::StringLiteral(literal) if wire => {
                return literal.value.as_str().map_or_else(
                    || RequestArgument::new(line, None),
                    |value| RequestArgument::wire_text(line, value),
                );
            }
            Expression::StringLiteral(_) | Expression::TemplateLiteral(_) if !wire => {
                Some(ArgumentType::String)
            }
            Expression::ObjectExpression(_) if !wire => Some(ArgumentType::Object),
            Expression::ArrayExpression(_) if !wire => Some(ArgumentType::Array),
            _ => None,
        };
        RequestArgument::new(line, kind)
    }

    // Records each call and retains unresolved diagnostics for dynamic destinations and methods.
    fn process_http_call(&mut self, call: &CallExpression<'_>) -> Option<(CallResultShape, usize)> {
        let target = self.classify_http_call(call)?;
        if !self.seen_call_spans.insert(call.span.start) {
            return None;
        }

        let (client_kind, method, result_shape) = match &target {
            CallTarget::Fetch(m) => (HttpClientKind::Fetch, *m, CallResultShape::FetchResponse),
            CallTarget::UseFetch(m) => {
                (HttpClientKind::Fetch, *m, CallResultShape::UseFetchResponse)
            }
            CallTarget::Axios(m) | CallTarget::AxiosInstance { method: m, .. } => (
                HttpClientKind::Axios,
                Some(*m),
                CallResultShape::AxiosResponse,
            ),
            CallTarget::Custom {
                method: m,
                is_use_fetch,
                ..
            } => (
                HttpClientKind::Custom,
                *m,
                if *is_use_fetch {
                    CallResultShape::UseFetchResponse
                } else {
                    CallResultShape::JsonPayload
                },
            ),
        };

        let (line, snippet) = self.location(call.span);
        let Some(method) = method else {
            self.unresolved_calls.push(ExtractedUnresolvedCall {
                client: client_kind,
                line,
                snippet,
                reason: "HTTP method cannot be resolved statically".into(),
            });
            return None;
        };
        let response_type = self
            .extract_response_type(call)
            .or_else(|| self.current_return_type());

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
                let request = self.request_evidence(call, &target, method, &url_str, line);
                let matched = match &target {
                    CallTarget::Fetch(_) | CallTarget::UseFetch(_) | CallTarget::Axios(_) => {
                        resolve_http_service_url(&url_str, self.ctx.services)
                    }
                    CallTarget::AxiosInstance { base_url, .. } => {
                        if is_absolute_http_url(&url_str) {
                            resolve_http_service_url(&url_str, self.ctx.services)
                        } else if let Some(base_url) = base_url {
                            let base = base_url.trim_end_matches('/');
                            let rel = url_str.trim_start_matches('/');
                            resolve_http_service_url(&format!("{base}/{rel}"), self.ctx.services)
                        } else {
                            Err("Axios instance base URL cannot be resolved statically".into())
                        }
                    }
                    CallTarget::Custom {
                        target_service,
                        path_prefix,
                        ..
                    } => {
                        if is_absolute_http_url(&url_str) {
                            resolve_http_service_url(&url_str, self.ctx.services)
                        } else {
                            Ok(Some(crate::url::ServiceEndpointMatch {
                                service: target_service.clone(),
                                normalized_path: normalize_relative_path(
                                    &url_str,
                                    path_prefix.as_deref(),
                                ),
                            }))
                        }
                    }
                };

                match matched {
                    Ok(Some(matched)) => {
                        let id = self.calls.len();
                        self.calls.push(ExtractedHttpCall {
                            id,
                            client: client_kind,
                            service: matched.service,
                            method,
                            normalized_path: matched.normalized_path,
                            response_type,
                            response_type_is_schema: false,
                            request,
                            line,
                            snippet,
                        });
                        Some((result_shape, id))
                    }
                    Ok(None) => None,
                    Err(reason) => {
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
            let Some(VarBinding::JsonPayload {
                call_id, prefix, ..
            }) = self.resolve_binding_from_expr(arg_expr)
            else {
                continue;
            };

            for hr in helper_reads.iter().filter(|r| r.param_index == arg_idx) {
                let full_path = join_field_path(&prefix, std::slice::from_ref(&hr.field_path));
                self.record_field_read(call_id, full_path, hr.span);
            }
        }
    }

    // Resuelve las URL de entorno sin incluir en el diagnóstico la clave sin configurar.
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
                    return UrlEval::Dynamic(UNMAPPED_ENV.into());
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

    // Resuelve plantillas de URL y comunica variables ausentes con un mensaje fijo.
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
                                return UrlEval::Dynamic(UNMAPPED_ENV.into());
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

                // Vue `ref(...)` / `shallowRef(...)`
                if is_vue_ref_call(call) {
                    let declared_type = self.extract_vue_ref_type(call);
                    if let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
                        && let Some(inner) = self.resolve_binding_from_expr(arg_expr)
                    {
                        match inner {
                            VarBinding::JsonPayload {
                                call_id, prefix, ..
                            } => {
                                if prefix.is_empty() {
                                    self.maybe_backfill_call_response_type(call_id, declared_type);
                                }
                                return Some(VarBinding::JsonPayload {
                                    call_id,
                                    prefix,
                                    is_vue_ref: true,
                                });
                            }
                            VarBinding::AxiosResponse { call_id, .. } => {
                                return Some(VarBinding::AxiosResponse {
                                    call_id,
                                    is_vue_ref: true,
                                });
                            }
                            VarBinding::FetchResponse { call_id, .. } => {
                                return Some(VarBinding::FetchResponse {
                                    call_id,
                                    is_vue_ref: true,
                                });
                            }
                            _ => {}
                        }
                    }
                    return Some(VarBinding::VueRefEmpty { declared_type });
                }

                // Zod `UserSchema.parse(data)` / `UserSchema.safeParse(data)`
                if let Some(schema_name) = crate::zod::extract_zod_parse_schema_reference(call)
                    && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
                    && let Some(inner) = self.resolve_binding_from_expr(arg_expr)
                {
                    let is_safe_parse = matches!(
                        unwrap_expr(&call.callee),
                        Expression::StaticMemberExpression(m)
                            if matches!(m.property.name.as_str(), "safeParse" | "safeParseAsync")
                    );
                    if let VarBinding::JsonPayload {
                        call_id,
                        prefix,
                        is_vue_ref,
                    } = inner
                    {
                        if prefix.is_empty() {
                            self.maybe_backfill_call_schema(call_id, schema_name);
                        }
                        if is_safe_parse {
                            return Some(VarBinding::AxiosResponse {
                                call_id,
                                is_vue_ref: false,
                            });
                        }
                        return Some(VarBinding::JsonPayload {
                            call_id,
                            prefix,
                            is_vue_ref,
                        });
                    }
                }

                // Vue `computed(() => expr)` returning a tracked payload or sub-field
                if is_vue_computed_call(call)
                    && let Some(Argument::ArrowFunctionExpression(arrow)) = call.arguments.first()
                {
                    let body_expr = match &arrow.body {
                        oxc_ast::ast::ArrowFunctionBody::FunctionBody(body) => {
                            if body.statements.len() == 1 {
                                match &body.statements[0] {
                                    Statement::ReturnStatement(ret) => ret.argument.as_ref(),
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        }
                        other => other.as_expression(),
                    };

                    if let Some(expr) = body_expr
                        && let Some(VarBinding::JsonPayload {
                            call_id, prefix, ..
                        }) = self.resolve_binding_from_expr(expr)
                    {
                        return Some(VarBinding::JsonPayload {
                            call_id,
                            prefix,
                            is_vue_ref: true,
                        });
                    }
                }

                // Direct `fetch(...)`, `useFetch(...)`, `axios.get(...)`, or configured custom client
                if let Some((shape, call_id)) = self.process_http_call(call) {
                    return Some(match shape {
                        CallResultShape::FetchResponse => VarBinding::FetchResponse {
                            call_id,
                            is_vue_ref: false,
                        },
                        CallResultShape::AxiosResponse => VarBinding::AxiosResponse {
                            call_id,
                            is_vue_ref: false,
                        },
                        CallResultShape::UseFetchResponse => VarBinding::UseFetchResponse(call_id),
                        // Custom clients like `$fetch<T>` / `getClient().fetch<T>` return parsed JSON directly.
                        CallResultShape::JsonPayload => VarBinding::JsonPayload {
                            call_id,
                            prefix: String::new(),
                            is_vue_ref: false,
                        },
                    });
                }

                // `res.json()` or `(await fetch(...)).json()`
                if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee)
                    && member.property.name == "json"
                    && let Some(VarBinding::FetchResponse { call_id, .. }) =
                        self.resolve_binding_from_expr(&member.object)
                {
                    return Some(VarBinding::JsonPayload {
                        call_id,
                        prefix: String::new(),
                        is_vue_ref: false,
                    });
                }
                None
            }
            Expression::Identifier(ident) => self.lookup_var(ident.name.as_str()).cloned(),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root_name, segments) = extract_member_chain(unwrapped)?;
                let root_binding = self.lookup_var(&root_name)?.clone();
                match root_binding {
                    VarBinding::FetchResponse {
                        call_id,
                        is_vue_ref: true,
                    } if segments.len() == 1 && segments[0] == "value" => {
                        Some(VarBinding::FetchResponse {
                            call_id,
                            is_vue_ref: false,
                        })
                    }
                    VarBinding::AxiosResponse {
                        call_id,
                        is_vue_ref,
                    } => {
                        let effective = if is_vue_ref
                            && segments.first().map(String::as_str) == Some("value")
                        {
                            &segments[1..]
                        } else {
                            &segments[..]
                        };
                        if effective.is_empty() {
                            return Some(VarBinding::AxiosResponse {
                                call_id,
                                is_vue_ref: false,
                            });
                        }
                        if effective[0] != "data" {
                            return None;
                        }
                        let field_segments = &effective[1..];
                        if field_segments.is_empty() {
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: String::new(),
                                is_vue_ref: false,
                            })
                        } else {
                            let field_path = field_segments.join(".");
                            self.record_field_read(call_id, field_path.clone(), unwrapped.span());
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: field_path,
                                is_vue_ref: false,
                            })
                        }
                    }
                    VarBinding::UseFetchResponse(call_id) => {
                        if segments.first().map(String::as_str) != Some("data") {
                            return None;
                        }
                        let after_data = &segments[1..];
                        if after_data.is_empty() {
                            return Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: String::new(),
                                is_vue_ref: true,
                            });
                        }
                        let field_segments =
                            if after_data.first().map(String::as_str) == Some("value") {
                                &after_data[1..]
                            } else {
                                after_data
                            };
                        if field_segments.is_empty() {
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: String::new(),
                                is_vue_ref: false,
                            })
                        } else {
                            let field_path = field_segments.join(".");
                            self.record_field_read(call_id, field_path.clone(), unwrapped.span());
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: field_path,
                                is_vue_ref: false,
                            })
                        }
                    }
                    VarBinding::JsonPayload {
                        call_id,
                        prefix,
                        is_vue_ref,
                    } => {
                        let effective = if is_vue_ref
                            && segments.first().map(String::as_str) == Some("value")
                        {
                            &segments[1..]
                        } else {
                            &segments[..]
                        };
                        if effective.is_empty() {
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix,
                                is_vue_ref: false,
                            })
                        } else {
                            let full_path = join_field_path(&prefix, effective);
                            self.record_field_read(call_id, full_path.clone(), unwrapped.span());
                            Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: full_path,
                                is_vue_ref: false,
                            })
                        }
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
                        VarBinding::AxiosResponse { call_id, .. } if key_name == "data" => {
                            self.bind_pattern(
                                &prop.value,
                                VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: String::new(),
                                    is_vue_ref: false,
                                },
                            );
                        }
                        VarBinding::UseFetchResponse(call_id) if key_name == "data" => {
                            self.bind_pattern(
                                &prop.value,
                                VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: String::new(),
                                    is_vue_ref: true,
                                },
                            );
                        }
                        VarBinding::JsonPayload {
                            call_id, prefix, ..
                        } => {
                            let field_path = join_field_path(prefix, &[key_name]);
                            self.record_field_read(*call_id, field_path.clone(), prop.span);
                            self.bind_pattern(
                                &prop.value,
                                VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: field_path,
                                    is_vue_ref: false,
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
        if self.seen_member_spans.contains(&span.start) {
            return;
        }
        let Some((root_name, segments)) = extract_member_chain(expr) else {
            return;
        };
        self.seen_member_spans.insert(span.start);
        mark_inner_member_spans(expr, &mut self.seen_member_spans);

        let Some(root_binding) = self.lookup_var(&root_name).cloned() else {
            return;
        };

        match root_binding {
            VarBinding::JsonPayload {
                call_id,
                prefix,
                is_vue_ref,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        &segments[..]
                    };
                if !effective.is_empty() {
                    let full_path = join_field_path(&prefix, effective);
                    self.record_field_read(call_id, full_path, span);
                }
            }
            VarBinding::AxiosResponse {
                call_id,
                is_vue_ref,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        &segments[..]
                    };
                if effective.first().map(String::as_str) == Some("data") && effective.len() > 1 {
                    let full_path = effective[1..].join(".");
                    self.record_field_read(call_id, full_path, span);
                }
            }
            VarBinding::UseFetchResponse(call_id)
                if segments.first().map(String::as_str) == Some("data") && segments.len() > 1 =>
            {
                let after_data = &segments[1..];
                let effective = if after_data.first().map(String::as_str) == Some("value") {
                    &after_data[1..]
                } else {
                    after_data
                };
                if !effective.is_empty() {
                    let full_path = effective.join(".");
                    self.record_field_read(call_id, full_path, span);
                }
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for FileExtractor<'_> {
    /// Keeps function parameters from resolving to an imported instance with the same name.
    fn visit_formal_parameters(&mut self, params: &FormalParameters<'a>) {
        for param in &params.items {
            self.register_unassigned_pattern(&param.pattern);
        }
        if let Some(rest) = &params.rest {
            self.register_unassigned_pattern(&rest.rest.argument);
        }
        walk::walk_formal_parameters(self, params);
    }

    fn visit_block_statement(&mut self, it: &oxc_ast::ast::BlockStatement<'a>) {
        self.push_scope();
        walk::walk_block_statement(self, it);
        self.pop_scope();
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: oxc_semantic::ScopeFlags) {
        let ret = self.extract_fn_return_type(it.return_type.as_deref());
        self.return_type_stack.push(ret);
        self.push_scope();
        walk::walk_function(self, it, flags);
        self.pop_scope();
        self.return_type_stack.pop();
    }

    fn visit_arrow_function_expression(&mut self, it: &oxc_ast::ast::ArrowFunctionExpression<'a>) {
        let ret = self.extract_fn_return_type(it.return_type.as_deref());
        self.return_type_stack.push(ret);
        self.push_scope();
        walk::walk_arrow_function_expression(self, it);
        self.pop_scope();
        self.return_type_stack.pop();
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(init) = &decl.init {
            if let Some(binding) = self.resolve_binding_from_expr(init) {
                self.bind_pattern(&decl.id, binding);
                if matches!(
                    unwrap_expr(init),
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
                ) {
                    self.seen_member_spans
                        .insert(unwrap_expr(init).span().start);
                    mark_inner_member_spans(init, &mut self.seen_member_spans);
                }
            } else {
                self.register_unassigned_pattern(&decl.id);
            }
        } else {
            self.register_unassigned_pattern(&decl.id);
        }
        walk::walk_variable_declarator(self, decl);
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        if matches!(
            it.operator,
            AssignmentOperator::Assign
                | AssignmentOperator::LogicalNullish
                | AssignmentOperator::LogicalOr
        ) && let Some((target_name, is_vue_ref_assign)) = extract_assignment_target(&it.left)
        {
            if let Some(binding) = self.resolve_binding_from_expr(&it.right) {
                let updated =
                    self.apply_ref_assignment_binding(&target_name, is_vue_ref_assign, binding);
                self.update_or_bind_var(&target_name, updated);
                if matches!(
                    unwrap_expr(&it.right),
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
                ) {
                    self.seen_member_spans
                        .insert(unwrap_expr(&it.right).span().start);
                    mark_inner_member_spans(&it.right, &mut self.seen_member_spans);
                }
            }
            if is_vue_ref_assign {
                self.seen_member_spans.insert(it.left.span().start);
            }
        }
        walk::walk_assignment_expression(self, it);
    }

    /// Records HTTP calls and associates Zod validators with their response.
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let _ = self.process_http_call(call);
        if let Some(schema_name) = crate::zod::extract_zod_parse_schema_reference(call)
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(VarBinding::JsonPayload {
                call_id, prefix, ..
            }) = self.resolve_binding_from_expr(arg_expr)
            && prefix.is_empty()
        {
            self.maybe_backfill_call_schema(call_id, schema_name);
        }
        self.process_helper_call(call);
        walk::walk_call_expression(self, call);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if matches!(
            expr,
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
        ) {
            self.inspect_member_chain(expr);
        }
        walk::walk_expression(self, expr);
    }
}

fn normalize_type_expr(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_outer_promise(raw: &str) -> &str {
    let mut current = raw.trim();
    loop {
        if let Some(inner) = current
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            && has_balanced_delimiters(inner)
        {
            current = inner.trim();
            continue;
        }
        if let Some(inner) = current
            .strip_prefix("Promise<")
            .and_then(|rest| rest.strip_suffix('>'))
            && has_balanced_delimiters(inner)
        {
            current = inner.trim();
            continue;
        }
        break;
    }
    current
}

fn has_balanced_delimiters(s: &str) -> bool {
    let mut angle: isize = 0;
    let mut paren: isize = 0;
    let mut brace: isize = 0;
    let mut bracket: isize = 0;

    for b in s.bytes() {
        match b {
            b'<' => angle += 1,
            b'>' => {
                angle -= 1;
                if angle < 0 {
                    return false;
                }
            }
            b'(' => paren += 1,
            b')' => {
                paren -= 1;
                if paren < 0 {
                    return false;
                }
            }
            b'{' => brace += 1,
            b'}' => {
                brace -= 1;
                if brace < 0 {
                    return false;
                }
            }
            b'[' => bracket += 1,
            b']' => {
                bracket -= 1;
                if bracket < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }

    angle == 0 && paren == 0 && brace == 0 && bracket == 0
}

fn split_top_level<'a>(s: &'a str, delims: &[u8]) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut angle: usize = 0;
    let mut paren: usize = 0;
    let mut brace: usize = 0;
    let mut bracket: usize = 0;
    let mut start = 0;

    for (idx, b) in s.bytes().enumerate() {
        match b {
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => brace = brace.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            _ if angle == 0 && paren == 0 && brace == 0 && bracket == 0 && delims.contains(&b) => {
                parts.push(s[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim());
    parts
}

fn is_non_dto_return_type(ty: &str) -> bool {
    let trimmed = ty.trim();
    if trimmed.is_empty() {
        return true;
    }
    let parts = split_top_level(trimmed, b"|&");
    parts.iter().all(|part| is_primitive_type_atom(part))
}

fn is_primitive_type_atom(raw: &str) -> bool {
    let mut current = raw.trim();
    loop {
        if let Some(inner) = current
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            && has_balanced_delimiters(inner)
        {
            current = inner.trim();
            continue;
        }
        if let Some(inner) = current.strip_suffix("[]") {
            current = inner.trim();
            continue;
        }
        if let Some(inner) = current.strip_prefix("readonly ") {
            current = inner.trim();
            continue;
        }
        if let Some(inner) = current
            .strip_prefix("Readonly<")
            .and_then(|rest| rest.strip_suffix('>'))
            .or_else(|| {
                current
                    .strip_prefix("Array<")
                    .and_then(|rest| rest.strip_suffix('>'))
            })
            .or_else(|| {
                current
                    .strip_prefix("ReadonlyArray<")
                    .and_then(|rest| rest.strip_suffix('>'))
            })
            && has_balanced_delimiters(inner)
        {
            current = inner.trim();
            continue;
        }
        break;
    }

    if current.is_empty() {
        return true;
    }

    let nested = split_top_level(current, b"|&");
    if nested.len() > 1 {
        return nested.iter().all(|part| is_primitive_type_atom(part));
    }

    if (current.starts_with('\'') && current.ends_with('\''))
        || (current.starts_with('"') && current.ends_with('"'))
        || current.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return true;
    }

    matches!(
        current,
        "string"
            | "number"
            | "boolean"
            | "void"
            | "null"
            | "undefined"
            | "any"
            | "unknown"
            | "never"
            | "bigint"
            | "symbol"
            | "true"
            | "false"
            | "Blob"
            | "Response"
            | "ArrayBuffer"
            | "Uint8Array"
            | "FormData"
    )
}

fn is_use_fetch_callee(callee: &str) -> bool {
    callee == "useFetch"
        || callee == "useLazyFetch"
        || callee.ends_with(".useFetch")
        || callee.ends_with(".useLazyFetch")
}

fn is_vue_ref_call(call: &CallExpression<'_>) -> bool {
    match unwrap_expr(&call.callee) {
        Expression::Identifier(ident) => ident.name == "ref" || ident.name == "shallowRef",
        Expression::StaticMemberExpression(member) => {
            member.property.name == "ref" || member.property.name == "shallowRef"
        }
        _ => false,
    }
}

fn is_vue_computed_call(call: &CallExpression<'_>) -> bool {
    match unwrap_expr(&call.callee) {
        Expression::Identifier(ident) => ident.name == "computed",
        Expression::StaticMemberExpression(member) => member.property.name == "computed",
        _ => false,
    }
}

fn extract_assignment_target(target: &AssignmentTarget<'_>) -> Option<(String, bool)> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            Some((ident.name.to_string(), false))
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            if member.property.name == "value"
                && let Expression::Identifier(ident) = unwrap_expr(&member.object)
            {
                return Some((ident.name.to_string(), true));
            }
            None
        }
        AssignmentTarget::TSNonNullExpression(ts) => extract_expr_assignment_target(&ts.expression),
        AssignmentTarget::TSAsExpression(ts) => extract_expr_assignment_target(&ts.expression),
        AssignmentTarget::TSSatisfiesExpression(ts) => {
            extract_expr_assignment_target(&ts.expression)
        }
        AssignmentTarget::TSTypeAssertion(ts) => extract_expr_assignment_target(&ts.expression),
        _ => None,
    }
}

fn extract_expr_assignment_target(expr: &Expression<'_>) -> Option<(String, bool)> {
    match unwrap_expr(expr) {
        Expression::Identifier(ident) => Some((ident.name.to_string(), false)),
        Expression::StaticMemberExpression(member) => {
            if member.property.name == "value"
                && let Expression::Identifier(ident) = unwrap_expr(&member.object)
            {
                return Some((ident.name.to_string(), true));
            }
            None
        }
        _ => None,
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

// Unwrap syntax that preserves the underlying expression.
fn unwrap_expr<'a, 'b>(mut expr: &'a Expression<'b>) -> &'a Expression<'b> {
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

// Distinguish missing arguments from positions that could be filled by a spread.
fn call_argument<'a, 'b>(
    call: &'a CallExpression<'b>,
    index: usize,
) -> Option<Option<&'a Expression<'b>>> {
    if call
        .arguments
        .iter()
        .take(index + 1)
        .any(|arg| arg.as_expression().is_none())
    {
        return None;
    }
    Some(call.arguments.get(index).and_then(Argument::as_expression))
}

// Find the last definition of an option while accounting for spreads and computed keys.
fn static_option<'a, 'b>(
    call: &'a CallExpression<'b>,
    index: usize,
    option: &str,
) -> Option<Option<&'a Expression<'b>>> {
    let Some(value) = call_argument(call, index)? else {
        return Some(None);
    };
    let options = unwrap_expr(value);
    if matches!(options, Expression::NullLiteral(_)) {
        return Some(None);
    }
    let Expression::ObjectExpression(object) = options else {
        return None;
    };
    for property in object.properties.iter().rev() {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return None;
        };
        match property_key_name(&property.key) {
            Some(name) if name == option => return Some(Some(&property.value)),
            None => return None,
            _ => {}
        }
    }
    Some(None)
}

// Recognize direct JSON serialization without a replacer.
fn json_stringify_input<'a, 'b>(expr: &'a Expression<'b>) -> Option<&'a Expression<'b>> {
    let Expression::CallExpression(call) = unwrap_expr(expr) else {
        return None;
    };
    let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) else {
        return None;
    };
    let Expression::Identifier(object) = unwrap_expr(&member.object) else {
        return None;
    };
    if object.name != "JSON" || member.property.name != "stringify" || call.arguments.len() != 1 {
        return None;
    }
    call.arguments.first()?.as_expression()
}

// Usa GET para opciones ausentes y conserva como desconocido cualquier método que pueda variar.
pub(crate) fn options_http_method(call: &CallExpression<'_>) -> Option<HttpMethod> {
    let Some(second_arg) = call.arguments.get(1) else {
        return Some(HttpMethod::Get);
    };
    let options = unwrap_expr(second_arg.as_expression()?);
    let Expression::ObjectExpression(object) = options else {
        return matches!(options, Expression::NullLiteral(_)).then_some(HttpMethod::Get);
    };

    // La última propiedad prevalece e incluso un spread puede sustituir un método anterior.
    for property in object.properties.iter().rev() {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            return None;
        };
        match property_key_name(&property.key).as_deref() {
            Some("method") => {
                let Expression::StringLiteral(literal) = unwrap_expr(&property.value) else {
                    return None;
                };
                return HttpMethod::parse(literal.value.as_str()?);
            }
            None if property.key.is_computed() => return None,
            _ => {}
        }
    }
    Some(HttpMethod::Get)
}

// Devuelve el nombre de una clave que se conoce sin evaluar código.
fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(ident) => Some(ident.name.to_string()),
        PropertyKey::StaticStringLiteral(lit) | PropertyKey::StringLiteral(lit) => {
            lit.value.as_str().map(ToOwned::to_owned)
        }
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

    // Verify that static keys after opaque entries remain usable and ambiguous options stay unknown.
    #[test]
    fn extracts_static_request_arguments_and_preserves_opaque_sections() {
        use laine_core::request::ArgumentCompatibility::{Compatible, Incompatible, Unknown};

        let source = r#"import axios from 'axios';
fetch('http://customer-service/spread', {
  method: 'POST',
  body: JSON.stringify({ lost: 'value', ...options, kept: 3, ['fixed']: true }),
  headers: { 'X-Mode': 'fast' },
});
fetch('http://customer-service/computed', {
  method: 'POST',
  body: JSON.stringify({ lost: 'value', [key]: value, kept: false }),
  headers: new Headers({ 'X-Mode': 'fast' }),
});
fetch('http://customer-service/opaque', { body: JSON.stringify({ tenant: 3 }), ...opts, method: 'POST' });
axios.get('http://customer-service/query?limit=3', { params: { limit: 2, page: 1 } });
useFetch('http://customer-service/nuxt', { query: { limit: 1 }, params: { limit: 'text', page: 2 } });
fetch(`http://customer-service/dynamic?limit=${value}`);
fetch('http://customer-service/duplicate', { headers: { 'X-Mode': '1', 'x-mode': 'true' } });
"#;
        let result = extract_file("api.ts", source, &customer_services()).unwrap();
        let [spread, computed, opaque, axios, nuxt, dynamic, duplicate] = result.calls.as_slice()
        else {
            panic!("unexpected calls: {:?}", result.calls);
        };
        assert_eq!(
            spread
                .request
                .body
                .compare(Some("lost"), true, "string", spread.line),
            Unknown(Some(4))
        );
        assert_eq!(
            spread
                .request
                .body
                .compare(Some("kept"), true, "integer", spread.line),
            Compatible
        );
        assert_eq!(
            spread
                .request
                .body
                .compare(Some("fixed"), true, "boolean", spread.line),
            Compatible
        );
        assert_eq!(
            spread
                .request
                .headers
                .compare(Some("x-mode"), true, "string", spread.line),
            Compatible
        );
        assert_eq!(
            computed
                .request
                .body
                .compare(Some("lost"), true, "string", computed.line),
            Unknown(Some(9))
        );
        assert_eq!(
            computed
                .request
                .body
                .compare(Some("kept"), true, "string", computed.line),
            Incompatible(9)
        );
        assert_eq!(
            computed
                .request
                .headers
                .compare(Some("x-mode"), true, "string", computed.line),
            Unknown(Some(10))
        );
        assert_eq!(
            opaque
                .request
                .body
                .compare(Some("tenant"), true, "integer", opaque.line),
            Unknown(None)
        );
        assert_eq!(
            axios
                .request
                .query
                .compare(Some("limit"), true, "integer", axios.line),
            Unknown(Some(13))
        );
        assert_eq!(
            axios
                .request
                .query
                .compare(Some("page"), true, "integer", axios.line),
            Compatible
        );
        assert_eq!(
            nuxt.request
                .query
                .compare(Some("limit"), true, "integer", nuxt.line),
            Compatible
        );
        assert_eq!(
            nuxt.request
                .query
                .compare(Some("page"), true, "integer", nuxt.line),
            Compatible
        );
        assert_eq!(
            dynamic
                .request
                .query
                .compare(Some("limit"), true, "integer", dynamic.line),
            Unknown(None)
        );
        assert_eq!(
            duplicate
                .request
                .headers
                .compare(Some("x-mode"), true, "boolean", duplicate.line),
            Unknown(Some(16))
        );
    }

    // Distingue GET por defecto, métodos literales y opciones capaces de sobrescribirlos.
    #[test]
    fn reports_unknown_methods_without_linking_them_to_get() {
        let source = r#"fetch('http://customer-service/default');
fetch('http://customer-service/options', { headers: {} });
fetch('http://customer-service/known', { ...options, method: 'POST' });
fetch('http://customer-service/dynamic', { method: verb });
fetch('http://customer-service/override', { method: 'POST', ...options });
fetch('http://customer-service/computed', { [key]: verb });
fetch('http://customer-service/opaque', options);
useFetch('http://customer-service/composable', { method: verb });
"#;
        let result = extract_file("api.ts", source, &customer_services()).unwrap();
        let calls = result
            .calls
            .iter()
            .map(|call| (call.normalized_path.as_str(), call.method))
            .collect::<Vec<_>>();
        assert_eq!(
            calls,
            [
                ("/default", HttpMethod::Get),
                ("/options", HttpMethod::Get),
                ("/known", HttpMethod::Post)
            ]
        );
        assert_eq!(
            result
                .unresolved_calls
                .iter()
                .map(|call| call.line)
                .collect::<Vec<_>>(),
            [4, 5, 6, 7, 8]
        );
        assert!(
            result
                .unresolved_calls
                .iter()
                .all(|call| call.reason == "HTTP method cannot be resolved statically")
        );
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
    // Verifica el análisis de clientes y el diagnóstico saneado de variables ausentes.
    fn extracts_custom_clients_axios_create_env_vars_and_local_helpers() {
        let services = customer_services();
        let clients = vec![
            CustomClientConfig {
                consumer_service: None,
                callee: "getClient().fetch".into(),
                target_service: "customer-service".into(),
                path_prefix: Some("/api".into()),
            },
            CustomClientConfig {
                consumer_service: None,
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
        assert_eq!(extracted.unresolved_calls[0].reason, UNMAPPED_ENV);
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
        assert_eq!(ship.calls[0].response_type, None);
        assert_eq!(ship.field_reads.len(), 1);
        assert_eq!(ship.field_reads[0].field_path, "address");
        assert!(!ship.field_reads.iter().any(|r| r.field_path == "fullName"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn captures_function_method_and_arrow_return_types_while_skipping_primitives() {
        let services = customer_services();
        let clients = vec![CustomClientConfig {
            consumer_service: None,
            callee: "getClient().fetch".into(),
            target_service: "customer-service".into(),
            path_prefix: Some("/api".into()),
        }];
        let empty_env = BTreeMap::new();

        let code = r#"
import axios from "axios";

export async function getUser(id: string): Promise<UserResponse> {
  const res = await fetch(`http://customer-service/customers/${id}`);
  return res.json();
}

export async function getInvoiceCustomer(id: string): Promise<string> {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const customer = await res.json();
  return `Invoice for ${customer.fullName}`;
}

export const userApi = {
  getUser(id: string): Promise<UserResponse> {
    return getClient().fetch('/users/' + id);
  },
  async deleteUser(id: string): Promise<void> {
    await getClient().fetch('/users/' + id, { method: 'DELETE' });
  },
  async downloadAvatar(id: string): Promise<Blob> {
    return getClient().fetch(`/users/${id}/avatar`);
  },
  async rawResponse(id: string): Promise<Response> {
    return fetch(`http://customer-service/customers/${id}`);
  },
};

export class CustomerClient {
  fetchNullable(id: string): Promise<UserResponse | null> {
    return getClient().fetch('/customers/' + id);
  }
}

export const arrowFetch = async (id: string): Promise<CustomerSummary[]> => {
  const { data } = await axios.get(`http://customer-service/customers/${id}`);
  return data;
};

export async function explicitGenericWins(id: string): Promise<UserViewModel> {
  const { data } = await axios.get<ExplicitApiDTO>(`http://customer-service/customers/${id}`);
  return data;
}
"#;

        let extracted = extract_file_with_context(
            "src/userApi.ts",
            code,
            ExtractContext {
                services: &services,
                clients: &clients,
                env: &empty_env,
            },
        )
        .expect("TS with return types should extract");

        assert_eq!(extracted.calls.len(), 9);
        assert_eq!(
            extracted.calls[0].response_type.as_deref(),
            Some("UserResponse")
        );
        assert_eq!(extracted.calls[1].response_type, None); // Promise<string> skipped
        assert_eq!(
            extracted.calls[2].response_type.as_deref(),
            Some("UserResponse")
        );
        assert_eq!(extracted.calls[3].response_type, None); // Promise<void> skipped
        assert_eq!(extracted.calls[4].response_type, None); // Promise<Blob> skipped
        assert_eq!(extracted.calls[5].response_type, None); // Promise<Response> skipped
        assert_eq!(
            extracted.calls[6].response_type.as_deref(),
            Some("UserResponse | null")
        );
        assert_eq!(
            extracted.calls[7].response_type.as_deref(),
            Some("CustomerSummary[]")
        );
        assert_eq!(
            extracted.calls[8].response_type.as_deref(),
            Some("ExplicitApiDTO")
        );
    }

    #[test]
    fn tracks_vue_ref_assignments_and_extracts_template_field_reads_with_exact_lines() {
        let sfc = concat!(
            "<script setup lang=\"ts\">\n", // line 1
            "import { ref } from 'vue';\n", // line 2
            "\n",                           // line 3
            "const user = ref(null);\n",    // line 4
            "const res = await fetch('http://customer-service/customers/42');\n", // line 5
            "user.value = await res.json();\n", // line 6
            "console.log(user.value.primerApellido);\n", // line 7
            "</script>\n",                  // line 8
            "\n",                           // line 9
            "<template>\n",                 // line 10
            "  <p>{{ user.segundoApellido }}</p>\n", // line 11
            "  <span :title=\"user?.email\"></span>\n", // line 12
            "</template>\n",                // line 13
        );

        let extracted = extract_file("src/components/UserProfile.vue", sfc, &customer_services())
            .expect("Vue SFC with ref and template reads should extract");

        assert_eq!(extracted.calls.len(), 1);
        assert_eq!(extracted.calls[0].id, 0);
        assert_eq!(extracted.calls[0].normalized_path, "/customers/42");
        assert_eq!(extracted.calls[0].line, 5);

        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();

        assert!(reads.contains(&(0, "primerApellido", 7)));
        assert!(reads.contains(&(0, "segundoApellido", 11)));
        assert!(reads.contains(&(0, "email", 12)));
        assert!(
            !extracted
                .field_reads
                .iter()
                .any(|r| r.field_path == "value" || r.field_path.starts_with("value.")),
            "Vue ref `.value` wrapper must never leak into field_path: {:?}",
            extracted.field_reads
        );

        let segundo = extracted
            .field_reads
            .iter()
            .find(|r| r.field_path == "segundoApellido")
            .unwrap();
        assert_eq!(segundo.snippet, "<p>{{ user.segundoApellido }}</p>");

        let email = extracted
            .field_reads
            .iter()
            .find(|r| r.field_path == "email")
            .unwrap();
        assert_eq!(email.snippet, "<span :title=\"user?.email\"></span>");
    }

    #[test]
    fn tracks_nested_ref_assignment_shallow_ref_use_fetch_and_plain_let_assignment() {
        let services = customer_services();
        let clients = vec![CustomClientConfig {
            consumer_service: None,
            callee: "getClient().fetch".into(),
            target_service: "customer-service".into(),
            path_prefix: None,
        }];
        let empty_env = BTreeMap::new();

        let sfc = concat!(
            "<script lang=\"ts\">\n",              // line 1
            "export async function preload() {\n", // line 2
            "  let legacy;\n",                     // line 3
            "  const res = await fetch('http://customer-service/customers/1');\n", // line 4
            "  legacy = await res.json();\n",      // line 5
            "  return legacy.legacyField;\n",      // line 6
            "}\n",                                 // line 7
            "</script>\n",                         // line 8
            "\n",                                  // line 9
            "<script setup lang=\"ts\">\n",        // line 10
            "import { ref, shallowRef } from 'vue';\n", // line 11
            "const user = ref<UserDTO | null>(null);\n", // line 12
            "const order = shallowRef<OrderDTO>();\n", // line 13
            "const { data: profile } = await useFetch<ProfileDTO>('http://customer-service/customers/9');\n", // line 14
            "\n",                                                                      // line 15
            "async function loadAll(id: string): Promise<void> {\n",                   // line 16
            "  const res = await fetch(`http://customer-service/customers/${id}`);\n", // line 17
            "  user.value = await res.json();\n",                                      // line 18
            "  order.value = await getClient().fetch('/orders/' + id);\n",             // line 19
            "  console.log(profile.value?.displayName);\n",                            // line 20
            "}\n",                                                                     // line 21
            "</script>\n",                                                             // line 22
            "\n",                                                                      // line 23
            "<template>\n",                                                            // line 24
            "  <div v-if=\"user?.active\">\n",                                         // line 25
            "    <p>{{ user.segundoApellido }}</p>\n",                                 // line 26
            "    <p>{{ profile.avatarUrl }}</p>\n",                                    // line 27
            "    <ul><li v-for=\"item in order.items\">{{ item.sku }}</li></ul>\n",    // line 28
            "  </div>\n",                                                              // line 29
            "</template>\n",                                                           // line 30
        );

        let extracted = extract_file_with_context(
            "src/views/Dashboard.vue",
            sfc,
            ExtractContext {
                services: &services,
                clients: &clients,
                env: &empty_env,
            },
        )
        .expect("complex Vue SFC should extract");

        // Call 0: <script> fetch(/customers/1)
        // Call 1: <script setup> useFetch(/customers/9) -> ProfileDTO
        // Call 2: <script setup> fetch(/customers/:id) -> backfilled UserDTO | null
        // Call 3: <script setup> getClient().fetch(/orders/:param) -> backfilled OrderDTO
        assert_eq!(extracted.calls.len(), 4);
        assert_eq!(extracted.calls[0].response_type, None);
        assert_eq!(
            extracted.calls[1].response_type.as_deref(),
            Some("ProfileDTO")
        );
        assert_eq!(
            extracted.calls[2].response_type.as_deref(),
            Some("UserDTO | null")
        );
        assert_eq!(
            extracted.calls[3].response_type.as_deref(),
            Some("OrderDTO")
        );

        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();

        assert!(reads.contains(&(0, "legacyField", 6)));
        assert!(reads.contains(&(1, "displayName", 20)));
        assert!(reads.contains(&(1, "avatarUrl", 27)));
        assert!(reads.contains(&(2, "active", 25)));
        assert!(reads.contains(&(2, "segundoApellido", 26)));
        assert!(reads.contains(&(3, "items", 28)));
    }

    #[test]
    fn handles_template_before_script_setup_axios_ref_and_object_class_binding() {
        let sfc = concat!(
            "<template>\n",                                                              // line 1
            "  <div :class=\"{ active: user?.isActive }\">\n",                           // line 2
            "    <input v-model=\"user.email\" />\n",                                    // line 3
            "    <p>{{ rawResp.data.fullName }}</p>\n",                                  // line 4
            "  </div>\n",                                                                // line 5
            "</template>\n",                                                             // line 6
            "\n",                                                                        // line 7
            "<script setup lang=\"ts\">\n",                                              // line 8
            "const user = ref(null);\n",                                                 // line 9
            "const rawResp = await axios.get('http://customer-service/customers/5');\n", // line 10
            "user.value = rawResp.data;\n",                                              // line 11
            "</script>\n",                                                               // line 12
        );

        let extracted = extract_file("src/components/Inline.vue", sfc, &customer_services())
            .expect("Vue SFC with template-first and no imports should extract");

        assert_eq!(extracted.calls.len(), 1);
        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();

        assert!(reads.contains(&(0, "isActive", 2)));
        assert!(reads.contains(&(0, "email", 3)));
        assert!(reads.contains(&(0, "fullName", 4)));
    }
}
