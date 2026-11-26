use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use laine_core::model::{Confidence, HttpMethod};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrowFunctionBody, ArrowFunctionExpression, AssignmentExpression, AssignmentOperator,
    AssignmentTarget, BindingPattern, BlockStatement, CallExpression, ChainElement, Class,
    ClassElement, Declaration, ExportAllDeclaration, ExportDefaultDeclarationKind,
    ExportFromDeclaration, Expression, FormalParameters, Function, FunctionBody, ImportDeclaration,
    ImportDeclarationSpecifier, ImportOrExportKind, JSXAttributeItem, JSXAttributeName,
    JSXAttributeValue, JSXElementName, JSXMemberExpression, JSXMemberExpressionObject,
    JSXOpeningElement, LogicalOperator, ModuleExportName, ObjectExpression, ObjectPropertyKind,
    Program, PropertyKey, ReturnStatement, Statement, TemplateLiteral, VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_semantic::ScopeFlags;
use oxc_span::{GetSpan, SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::extractor::{ExtractContext, options_http_method};
use crate::url::{
    ServiceEndpointMatch, is_absolute_http_url, normalize_relative_path,
    resolve_url_against_services,
};
use crate::vue::{extract_vue_scripts_with_setup, extract_vue_template_expressions};
use crate::zod::extract_zod_parse_schema_name;

const MAX_PROPAGATION_HOPS: usize = 6;

/// A response field read discovered through summary-based interprocedural cross-file dataflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossFileFieldRead {
    pub service: String,
    pub method: HttpMethod,
    pub normalized_path: String,
    pub field_path: String,
    pub read_file: String,
    pub read_line: usize,
    pub read_snippet: String,
    pub origin_file: String,
    pub origin_line: usize,
    pub confidence: Confidence,
    /// Full multi-file dependency chain, e.g.:
    /// `["GET /users/:id", "HTTP call @ src/services/user.js:3", "export `getUser` -> src/hooks/useUser.js:4", "read `segundoApellido` @ src/components/UserCard.jsx:6"]`
    pub trace: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossFileAnalysis {
    pub field_reads: Vec<CrossFileFieldRead>,
    pub limited_files: BTreeSet<String>,
}

/// Runs a 3-pass summary-based interprocedural dataflow analysis across a set of JS/TS/JSX/TSX/Vue files:
///
/// 1. **Pass 1:** Extracts per-file `ModuleSummary` (imports, barrel re-exports, exported endpoint-returning
///    functions/service objects, framework data-fetching hooks, and exported parameter / JSX-prop field reads).
/// 2. **Pass 2:** Performs cycle-safe bounded fixed-point propagation (up to 6 hops) across barrel re-exports
///    and delegated function/hook calls.
/// 3. **Pass 3:** Tracks consumer-file reads (direct member access, optional chaining, destructuring, aliases,
///    Vue `ref` / `<template>` expressions) and downward propagation into imported helpers & JSX components.
pub fn analyze_cross_file_dataflow(
    files: &[(String, String)],
    ctx: ExtractContext<'_>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
) -> Vec<CrossFileFieldRead> {
    analyze_cross_file_dataflow_with_limits(files, ctx, resolve_import).field_reads
}

// Propaga la evidencia y señala los archivos donde quedó una transición fuera del límite.
pub fn analyze_cross_file_dataflow_with_limits(
    files: &[(String, String)],
    ctx: ExtractContext<'_>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
) -> CrossFileAnalysis {
    // Canonical deterministic ordering regardless of caller input order.
    let mut file_map: BTreeMap<&str, &str> = BTreeMap::new();
    for (path, source) in files {
        file_map.insert(path.as_str(), source.as_str());
    }

    // Pass 1: Extract per-file module summaries.
    let mut summaries: BTreeMap<String, ModuleSummary> = BTreeMap::new();
    for (&file_path, &source) in &file_map {
        let summary = extract_module_summary(file_path, source, ctx);
        summaries.insert(file_path.to_string(), summary);
    }

    // Pass 2: Bounded fixed-point propagation (max 6 hops, cycle-safe).
    let propagated = propagate_summaries(&summaries, resolve_import);

    // Pass 3: Consumer-file field reads, JSX props, Vue templates, and cross-file helper calls.
    let mut raw_reads = Vec::new();
    for (&file_path, &source) in &file_map {
        let Some(summary) = summaries.get(file_path) else {
            continue;
        };
        collect_consumer_reads(
            file_path,
            source,
            ctx,
            summary,
            &propagated,
            resolve_import,
            &mut raw_reads,
        );
    }

    CrossFileAnalysis {
        field_reads: dedup_and_sort_reads(raw_reads),
        limited_files: propagated.limited_files,
    }
}

// ---------------------------------------------------------------------------
// Internal Summary & Propagation Data Models
// ---------------------------------------------------------------------------

type EndpointMap = BTreeMap<(String, String), Vec<ResolvedExportEndpoint>>;
type ParamReadMap = BTreeMap<(String, String), Vec<ResolvedParamRead>>;
type LocalObjMethodsMap = BTreeMap<String, BTreeMap<String, Vec<ExportReturnSummary>>>;
type LocalObjParamReadsMap = BTreeMap<String, BTreeMap<String, Vec<ParamFieldRead>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportBinding {
    Named { specifier: String, imported: String },
    Namespace { specifier: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReExportTarget {
    Named { specifier: String, imported: String },
    Namespace { specifier: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PayloadProjection {
    /// Returned value is the JSON payload directly (or a Vue ref wrapping it).
    Direct { is_vue_ref: bool },
    /// Returned value is an object/wrapper whose property `prop` holds the JSON payload
    /// (e.g. `return { user, loading }`, `axios.get`, `useQuery`, `useSWR`, `useAsyncData`, `useFetch`).
    Property { prop: String, is_vue_ref: bool },
    /// Returned value is a raw `fetch` Response whose `.json()` yields the payload.
    FetchResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DirectEndpointInfo {
    service: String,
    method: HttpMethod,
    normalized_path: String,
    origin_line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DelegationTarget {
    Import {
        import_local: String,
        member: Option<String>,
    },
    SameFile {
        local_key: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportReturnSummary {
    Direct {
        endpoint: DirectEndpointInfo,
        projection: PayloadProjection,
        field_prefix: String,
        via_framework_hook: bool,
    },
    Delegated {
        target: DelegationTarget,
        source_prop: Option<String>,
        unwrapped_json: bool,
        return_projection: PayloadProjection,
        field_prefix: String,
        call_line: usize,
        via_framework_hook: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ParamFieldRead {
    param_index: usize,
    prop_name: Option<String>,
    field_path: String,
    line: usize,
    snippet: String,
}

#[derive(Debug, Clone, Default)]
struct ModuleSummary {
    imports: BTreeMap<String, ImportBinding>,
    reexports: BTreeMap<String, ReExportTarget>,
    star_reexports: Vec<String>,
    internal_returns: BTreeMap<String, Vec<ExportReturnSummary>>,
    export_returns: BTreeMap<String, Vec<ExportReturnSummary>>,
    export_param_reads: BTreeMap<String, Vec<ParamFieldRead>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolvedExportEndpoint {
    service: String,
    method: HttpMethod,
    normalized_path: String,
    origin_file: String,
    origin_line: usize,
    confidence: Confidence,
    projection: PayloadProjection,
    field_prefix: String,
    trace_steps: Vec<String>,
    hops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolvedParamRead {
    def_file: String,
    param_index: usize,
    prop_name: Option<String>,
    field_path: String,
    line: usize,
    snippet: String,
}

struct PropagatedSummaries {
    resolved_exports: EndpointMap,
    resolved_internal: EndpointMap,
    resolved_param_reads: ParamReadMap,
    limited_files: BTreeSet<String>,
}

#[derive(Debug, Clone)]
enum ModuleEnvVal {
    StringLiteral(String),
    HttpClientInstance { base_url: String },
}

#[derive(Debug, Clone)]
enum CallTarget {
    Fetch(HttpMethod),
    UseFetch(HttpMethod),
    Axios(HttpMethod),
    AxiosInstance {
        base_url: String,
        method: HttpMethod,
    },
    Custom {
        target_service: String,
        path_prefix: Option<String>,
        method: HttpMethod,
        is_use_fetch: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum FrameworkHookKind {
    ReactQuery,
    Swr,
    NuxtAsyncData,
}

impl FrameworkHookKind {
    fn is_vue_ref(self) -> bool {
        matches!(self, Self::NuxtAsyncData)
    }
}

struct ParsedBlock {
    code: String,
    source_type: SourceType,
    is_vue_setup: bool,
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

// ---------------------------------------------------------------------------
// Parsing Utilities
// ---------------------------------------------------------------------------

fn extract_parse_blocks(file_path: &str, source: &str) -> Vec<ParsedBlock> {
    if file_path.to_ascii_lowercase().ends_with(".vue") {
        return extract_vue_scripts_with_setup(source)
            .into_iter()
            .map(|(block, is_setup)| ParsedBlock {
                code: block.code,
                source_type: block.lang.source_type(),
                is_vue_setup: is_setup,
            })
            .collect();
    }

    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::tsx());
    vec![ParsedBlock {
        code: source.to_string(),
        source_type,
        is_vue_setup: false,
    }]
}

fn parse_program<'a>(
    allocator: &'a Allocator,
    code: &'a str,
    source_type: SourceType,
) -> Option<Program<'a>> {
    let parsed = Parser::new(allocator, code, source_type).parse();
    if parsed.diagnostics.is_empty() {
        return Some(parsed.program);
    }

    if !source_type.is_jsx() {
        let retry = Parser::new(allocator, code, source_type.with_jsx(true)).parse();
        if retry.diagnostics.is_empty() {
            return Some(retry.program);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Pass 1: Per-File ModuleSummary Extraction
// ---------------------------------------------------------------------------

fn extract_module_summary(file_path: &str, source: &str, ctx: ExtractContext<'_>) -> ModuleSummary {
    let mut summary = ModuleSummary::default();
    let blocks = extract_parse_blocks(file_path, source);

    for block in blocks {
        let allocator = Allocator::default();
        let Some(program) = parse_program(&allocator, &block.code, block.source_type) else {
            continue;
        };
        let line_index = LineIndex::new(&block.code);
        extract_block_summary(&program, &block.code, &line_index, ctx, &mut summary);
    }

    summary
}

fn extract_block_summary(
    program: &Program<'_>,
    source: &str,
    line_index: &LineIndex,
    ctx: ExtractContext<'_>,
    summary: &mut ModuleSummary,
) {
    let mut module_env: BTreeMap<String, ModuleEnvVal> = BTreeMap::new();
    let mut local_callables: BTreeSet<String> = BTreeSet::new();

    // Step 1a: Collect imports, re-exports, CommonJS requires, top-level constants/clients, and local callable names.
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                collect_import_declaration(decl, &mut summary.imports);
            }
            Statement::ExportFromDeclaration(decl) => {
                collect_export_from_declaration(decl, &mut summary.reexports);
            }
            Statement::ExportAllDeclaration(decl) => {
                collect_export_all_declaration(decl, summary);
            }
            Statement::VariableDeclaration(vd) => {
                for decl in &vd.declarations {
                    collect_cjs_require(decl, &mut summary.imports);
                    collect_top_level_env_or_callable(
                        decl,
                        ctx,
                        &mut module_env,
                        &mut local_callables,
                    );
                }
            }
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    local_callables.insert(id.name.to_string());
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    register_class_callable_names(id.name.as_str(), cls, &mut local_callables);
                }
            }
            Statement::ExportDeclaration(ed) => match &ed.declaration {
                Declaration::VariableDeclaration(vd) => {
                    for decl in &vd.declarations {
                        collect_top_level_env_or_callable(
                            decl,
                            ctx,
                            &mut module_env,
                            &mut local_callables,
                        );
                    }
                }
                Declaration::FunctionDeclaration(func) => {
                    if let Some(id) = &func.id {
                        local_callables.insert(id.name.to_string());
                    }
                }
                Declaration::ClassDeclaration(cls) => {
                    if let Some(id) = &cls.id {
                        register_class_callable_names(id.name.as_str(), cls, &mut local_callables);
                    }
                }
                _ => {}
            },
            Statement::ExportDefaultDeclaration(edd) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &edd.declaration
                    && let Some(id) = &func.id
                {
                    local_callables.insert(id.name.to_string());
                }
            }
            _ => {}
        }
    }

    // Step 1b: Summarize all top-level functions, arrow functions, service objects, and classes.
    let mut local_returns: BTreeMap<String, Vec<ExportReturnSummary>> = BTreeMap::new();
    let mut local_param_reads: BTreeMap<String, Vec<ParamFieldRead>> = BTreeMap::new();
    let mut local_obj_methods: LocalObjMethodsMap = BTreeMap::new();
    let mut local_obj_param_reads: LocalObjParamReadsMap = BTreeMap::new();
    let mut pending_obj_aliases: Vec<(String, String, String)> = Vec::new();
    let mut pending_obj_spreads: Vec<(String, String)> = Vec::new();
    let mut pending_named_exports: Vec<(String, String)> = Vec::new();
    let mut pending_default_ident: Option<String> = None;
    let mut pending_cjs_default_ident: Option<String> = None;

    let imports_snapshot = summary.imports.clone();
    let env_ctx = AnalyzerStaticContext {
        source,
        line_index,
        ctx,
        module_env: &module_env,
        imports: &imports_snapshot,
        local_callables: &local_callables,
    };

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    let name = id.name.to_string();
                    let returns = analyze_function_returns(func, &env_ctx);
                    if !returns.is_empty() {
                        local_returns.insert(name.clone(), returns);
                    }
                    let reads = analyze_function_param_reads(func, source, line_index);
                    if !reads.is_empty() {
                        local_param_reads.insert(name, reads);
                    }
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    analyze_class_methods(
                        id.name.as_str(),
                        cls,
                        &env_ctx,
                        &mut local_obj_methods,
                        &mut local_obj_param_reads,
                    );
                }
            }
            Statement::VariableDeclaration(vd) => {
                for decl in &vd.declarations {
                    analyze_top_level_declarator(
                        decl,
                        &env_ctx,
                        &mut local_returns,
                        &mut local_param_reads,
                        &mut local_obj_methods,
                        &mut local_obj_param_reads,
                        &mut pending_obj_aliases,
                        &mut pending_obj_spreads,
                    );
                }
            }
            Statement::ExportDeclaration(ed) => match &ed.declaration {
                Declaration::FunctionDeclaration(func) => {
                    if let Some(id) = &func.id {
                        let name = id.name.to_string();
                        let returns = analyze_function_returns(func, &env_ctx);
                        if !returns.is_empty() {
                            local_returns.insert(name.clone(), returns.clone());
                            extend_returns(&mut summary.export_returns, &name, returns);
                        }
                        let reads = analyze_function_param_reads(func, source, line_index);
                        if !reads.is_empty() {
                            local_param_reads.insert(name.clone(), reads.clone());
                            extend_param_reads(&mut summary.export_param_reads, &name, reads);
                        }
                    }
                }
                Declaration::ClassDeclaration(cls) => {
                    if let Some(id) = &cls.id {
                        let name = id.name.to_string();
                        analyze_class_methods(
                            &name,
                            cls,
                            &env_ctx,
                            &mut local_obj_methods,
                            &mut local_obj_param_reads,
                        );
                        pending_named_exports.push((name.clone(), name));
                    }
                }
                Declaration::VariableDeclaration(vd) => {
                    for decl in &vd.declarations {
                        analyze_top_level_declarator(
                            decl,
                            &env_ctx,
                            &mut local_returns,
                            &mut local_param_reads,
                            &mut local_obj_methods,
                            &mut local_obj_param_reads,
                            &mut pending_obj_aliases,
                            &mut pending_obj_spreads,
                        );
                        if let BindingPattern::BindingIdentifier(ident) = &decl.id {
                            let name = ident.name.to_string();
                            if let Some(init) = &decl.init
                                && let Expression::Identifier(rhs) =
                                    unwrap_hoc_expr(unwrap_expr(init))
                            {
                                let rhs_name = rhs.name.as_str();
                                if let Some(imp) = summary.imports.get(rhs_name).cloned() {
                                    register_import_reexport(&name, &imp, &mut summary.reexports);
                                } else {
                                    pending_named_exports.push((rhs_name.to_string(), name));
                                }
                            } else {
                                pending_named_exports.push((name.clone(), name));
                            }
                        }
                    }
                }
                _ => {}
            },
            Statement::ExportDefaultDeclaration(edd) => match &edd.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    let returns = analyze_function_returns(func, &env_ctx);
                    let reads = analyze_function_param_reads(func, source, line_index);
                    if let Some(id) = &func.id {
                        let local_name = id.name.to_string();
                        if !returns.is_empty() {
                            local_returns.insert(local_name.clone(), returns.clone());
                        }
                        if !reads.is_empty() {
                            local_param_reads.insert(local_name, reads.clone());
                        }
                    }
                    if !returns.is_empty() {
                        extend_returns(&mut summary.export_returns, "default", returns);
                    }
                    if !reads.is_empty() {
                        extend_param_reads(&mut summary.export_param_reads, "default", reads);
                    }
                }
                ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
                    let mut tmp_methods = BTreeMap::new();
                    let mut tmp_reads = BTreeMap::new();
                    analyze_class_methods(
                        "default",
                        cls,
                        &env_ctx,
                        &mut tmp_methods,
                        &mut tmp_reads,
                    );
                    if let Some(methods) = tmp_methods.remove("default") {
                        for (m, rets) in methods {
                            extend_returns(
                                &mut summary.export_returns,
                                &format!("default.{m}"),
                                rets,
                            );
                        }
                    }
                    if let Some(methods) = tmp_reads.remove("default") {
                        for (m, reads) in methods {
                            extend_param_reads(
                                &mut summary.export_param_reads,
                                &format!("default.{m}"),
                                reads,
                            );
                        }
                    }
                }
                other => {
                    if let Some(expr) = other.as_expression() {
                        let unwrapped = unwrap_hoc_expr(unwrap_expr(expr));
                        match unwrapped {
                            Expression::ArrowFunctionExpression(arrow) => {
                                let returns = analyze_arrow_returns(arrow, &env_ctx);
                                if !returns.is_empty() {
                                    extend_returns(&mut summary.export_returns, "default", returns);
                                }
                                let reads = analyze_arrow_param_reads(arrow, source, line_index);
                                if !reads.is_empty() {
                                    extend_param_reads(
                                        &mut summary.export_param_reads,
                                        "default",
                                        reads,
                                    );
                                }
                            }
                            Expression::FunctionExpression(func) => {
                                let returns = analyze_function_returns(func, &env_ctx);
                                if !returns.is_empty() {
                                    extend_returns(&mut summary.export_returns, "default", returns);
                                }
                                let reads = analyze_function_param_reads(func, source, line_index);
                                if !reads.is_empty() {
                                    extend_param_reads(
                                        &mut summary.export_param_reads,
                                        "default",
                                        reads,
                                    );
                                }
                            }
                            Expression::ObjectExpression(obj) => {
                                analyze_object_methods(
                                    "default",
                                    obj,
                                    &env_ctx,
                                    &mut local_obj_methods,
                                    &mut local_obj_param_reads,
                                    &mut pending_obj_aliases,
                                    &mut pending_obj_spreads,
                                );
                                pending_default_ident = Some("default".to_string());
                            }
                            Expression::Identifier(ident) => {
                                let name = ident.name.to_string();
                                if let Some(imp) = summary.imports.get(&name).cloned() {
                                    register_import_reexport(
                                        "default",
                                        &imp,
                                        &mut summary.reexports,
                                    );
                                } else {
                                    pending_default_ident = Some(name);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            },
            Statement::ExportNamedDeclaration(end) => {
                if end.export_kind == ImportOrExportKind::Type {
                    continue;
                }
                for spec in &end.specifiers {
                    if spec.export_kind == ImportOrExportKind::Type {
                        continue;
                    }
                    let Some(local_name) = module_export_name_str(&spec.local) else {
                        continue;
                    };
                    let Some(exported_name) = module_export_name_str(&spec.exported) else {
                        continue;
                    };
                    if let Some(imp) = summary.imports.get(&local_name).cloned() {
                        register_import_reexport(&exported_name, &imp, &mut summary.reexports);
                    } else {
                        pending_named_exports.push((local_name, exported_name));
                    }
                }
            }
            Statement::ExpressionStatement(expr_stmt) => {
                if let Expression::AssignmentExpression(assign) = unwrap_expr(&expr_stmt.expression)
                    && assign.operator == AssignmentOperator::Assign
                {
                    collect_cjs_export(
                        assign,
                        &env_ctx,
                        summary,
                        &mut local_obj_methods,
                        &mut local_obj_param_reads,
                        &mut pending_obj_aliases,
                        &mut pending_obj_spreads,
                        &mut pending_named_exports,
                        &mut pending_cjs_default_ident,
                    );
                }
            }
            _ => {}
        }
    }

    // Resolve shorthand / aliased properties on local service objects (e.g. `const userService = { getUser }`).
    for (obj_name, prop_name, target_ident) in pending_obj_aliases {
        if let Some(rets) = local_returns.get(&target_ident).cloned() {
            extend_returns(
                local_obj_methods.entry(obj_name.clone()).or_default(),
                &prop_name,
                rets,
            );
        } else if summary.imports.contains_key(&target_ident) {
            extend_returns(
                local_obj_methods.entry(obj_name.clone()).or_default(),
                &prop_name,
                vec![ExportReturnSummary::Delegated {
                    target: DelegationTarget::Import {
                        import_local: target_ident.clone(),
                        member: None,
                    },
                    source_prop: None,
                    unwrapped_json: false,
                    return_projection: PayloadProjection::Direct { is_vue_ref: false },
                    field_prefix: String::new(),
                    call_line: 1,
                    via_framework_hook: false,
                }],
            );
        }
        if let Some(reads) = local_param_reads.get(&target_ident).cloned() {
            extend_param_reads(
                local_obj_param_reads.entry(obj_name).or_default(),
                &prop_name,
                reads,
            );
        }
    }

    // Resolve object spreads (e.g. `const combined = { ...baseService }`).
    for (obj_name, spread_ident) in pending_obj_spreads {
        if let Some(methods) = local_obj_methods.get(&spread_ident).cloned() {
            let entry = local_obj_methods.entry(obj_name.clone()).or_default();
            for (m, rets) in methods {
                extend_returns(entry, &m, rets);
            }
        }
        if let Some(methods) = local_obj_param_reads.get(&spread_ident).cloned() {
            let entry = local_obj_param_reads.entry(obj_name).or_default();
            for (m, reads) in methods {
                extend_param_reads(entry, &m, reads);
            }
        }
    }

    // Record all internal returns (for same-file helper delegation).
    for (name, rets) in &local_returns {
        extend_returns(&mut summary.internal_returns, name, rets.clone());
    }
    for (obj_name, methods) in &local_obj_methods {
        for (m, rets) in methods {
            extend_returns(
                &mut summary.internal_returns,
                &format!("{obj_name}.{m}"),
                rets.clone(),
            );
        }
    }

    // Finalize named exports.
    for (local_name, exported_name) in pending_named_exports {
        if let Some(rets) = local_returns.get(&local_name).cloned() {
            extend_returns(&mut summary.export_returns, &exported_name, rets);
        }
        if let Some(reads) = local_param_reads.get(&local_name).cloned() {
            extend_param_reads(&mut summary.export_param_reads, &exported_name, reads);
        }
        if let Some(methods) = local_obj_methods.get(&local_name).cloned() {
            for (m, rets) in methods {
                extend_returns(
                    &mut summary.export_returns,
                    &format!("{exported_name}.{m}"),
                    rets,
                );
            }
        }
        if let Some(methods) = local_obj_param_reads.get(&local_name).cloned() {
            for (m, reads) in methods {
                extend_param_reads(
                    &mut summary.export_param_reads,
                    &format!("{exported_name}.{m}"),
                    reads,
                );
            }
        }
    }

    // Finalize default export identifier / object.
    if let Some(name) = pending_default_ident {
        if name != "default" {
            if let Some(rets) = local_returns.get(&name).cloned() {
                extend_returns(&mut summary.export_returns, "default", rets);
            }
            if let Some(reads) = local_param_reads.get(&name).cloned() {
                extend_param_reads(&mut summary.export_param_reads, "default", reads);
            }
        }
        if let Some(methods) = local_obj_methods.get(&name).cloned() {
            for (m, rets) in methods {
                extend_returns(&mut summary.export_returns, &format!("default.{m}"), rets);
            }
        }
        if let Some(methods) = local_obj_param_reads.get(&name).cloned() {
            for (m, reads) in methods {
                extend_param_reads(
                    &mut summary.export_param_reads,
                    &format!("default.{m}"),
                    reads,
                );
            }
        }
    }

    // Finalize CommonJS `module.exports = ...` (exports both as `default` and named properties).
    if let Some(name) = pending_cjs_default_ident {
        if name != "__cjs_default" {
            if let Some(rets) = local_returns.get(&name).cloned() {
                extend_returns(&mut summary.export_returns, "default", rets);
            }
            if let Some(reads) = local_param_reads.get(&name).cloned() {
                extend_param_reads(&mut summary.export_param_reads, "default", reads);
            }
        }
        if let Some(methods) = local_obj_methods.get(&name).cloned() {
            for (m, rets) in methods {
                extend_returns(&mut summary.export_returns, &m, rets.clone());
                extend_returns(&mut summary.export_returns, &format!("default.{m}"), rets);
            }
        }
        if let Some(methods) = local_obj_param_reads.get(&name).cloned() {
            for (m, reads) in methods {
                extend_param_reads(&mut summary.export_param_reads, &m, reads.clone());
                extend_param_reads(
                    &mut summary.export_param_reads,
                    &format!("default.{m}"),
                    reads,
                );
            }
        }
    }
}

fn collect_import_declaration(
    decl: &ImportDeclaration<'_>,
    imports: &mut BTreeMap<String, ImportBinding>,
) {
    if decl.import_kind == ImportOrExportKind::Type {
        return;
    }
    let Some(specifier) = decl.source.value.as_str().map(ToOwned::to_owned) else {
        return;
    };
    let Some(specifiers) = &decl.specifiers else {
        return;
    };

    for spec in specifiers {
        match spec {
            ImportDeclarationSpecifier::ImportSpecifier(named) => {
                if named.import_kind == ImportOrExportKind::Type {
                    continue;
                }
                if let Some(imported) = module_export_name_str(&named.imported) {
                    imports.insert(
                        named.local.name.to_string(),
                        ImportBinding::Named {
                            specifier: specifier.clone(),
                            imported,
                        },
                    );
                }
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(def) => {
                imports.insert(
                    def.local.name.to_string(),
                    ImportBinding::Named {
                        specifier: specifier.clone(),
                        imported: "default".to_string(),
                    },
                );
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(ns) => {
                imports.insert(
                    ns.local.name.to_string(),
                    ImportBinding::Namespace {
                        specifier: specifier.clone(),
                    },
                );
            }
        }
    }
}

fn collect_export_from_declaration(
    decl: &ExportFromDeclaration<'_>,
    reexports: &mut BTreeMap<String, ReExportTarget>,
) {
    if decl.export_kind == ImportOrExportKind::Type {
        return;
    }
    let Some(specifier) = decl.source.value.as_str().map(ToOwned::to_owned) else {
        return;
    };

    for spec in &decl.specifiers {
        if spec.export_kind == ImportOrExportKind::Type {
            continue;
        }
        let Some(imported) = module_export_name_str(&spec.local) else {
            continue;
        };
        let Some(exported) = module_export_name_str(&spec.exported) else {
            continue;
        };
        reexports.insert(
            exported,
            ReExportTarget::Named {
                specifier: specifier.clone(),
                imported,
            },
        );
    }
}

fn collect_export_all_declaration(decl: &ExportAllDeclaration<'_>, summary: &mut ModuleSummary) {
    if decl.export_kind == ImportOrExportKind::Type {
        return;
    }
    let Some(specifier) = decl.source.value.as_str().map(ToOwned::to_owned) else {
        return;
    };

    if let Some(exported) = &decl.exported {
        if let Some(exported_name) = module_export_name_str(exported) {
            summary
                .reexports
                .insert(exported_name, ReExportTarget::Namespace { specifier });
        }
    } else if !summary.star_reexports.contains(&specifier) {
        summary.star_reexports.push(specifier);
    }
}

fn register_import_reexport(
    exported_name: &str,
    imp: &ImportBinding,
    reexports: &mut BTreeMap<String, ReExportTarget>,
) {
    match imp {
        ImportBinding::Named {
            specifier,
            imported,
        } => {
            reexports.insert(
                exported_name.to_string(),
                ReExportTarget::Named {
                    specifier: specifier.clone(),
                    imported: imported.clone(),
                },
            );
        }
        ImportBinding::Namespace { specifier } => {
            reexports.insert(
                exported_name.to_string(),
                ReExportTarget::Namespace {
                    specifier: specifier.clone(),
                },
            );
        }
    }
}

fn collect_cjs_require(
    decl: &VariableDeclarator<'_>,
    imports: &mut BTreeMap<String, ImportBinding>,
) {
    let Some(init) = &decl.init else {
        return;
    };
    let Expression::CallExpression(call) = unwrap_expr(init) else {
        return;
    };
    let Expression::Identifier(callee) = unwrap_expr(&call.callee) else {
        return;
    };
    if callee.name != "require" {
        return;
    }
    let Some(Expression::StringLiteral(lit)) = call
        .arguments
        .first()
        .and_then(Argument::as_expression)
        .map(unwrap_expr)
    else {
        return;
    };
    let Some(specifier) = lit.value.as_str().map(ToOwned::to_owned) else {
        return;
    };

    match &decl.id {
        BindingPattern::BindingIdentifier(ident) => {
            imports.insert(
                ident.name.to_string(),
                ImportBinding::Named {
                    specifier,
                    imported: "default".to_string(),
                },
            );
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                let Some(imported) = property_key_name(&prop.key) else {
                    continue;
                };
                if let BindingPattern::BindingIdentifier(local) = &prop.value {
                    imports.insert(
                        local.name.to_string(),
                        ImportBinding::Named {
                            specifier: specifier.clone(),
                            imported,
                        },
                    );
                }
            }
        }
        _ => {}
    }
}

fn collect_top_level_env_or_callable(
    decl: &VariableDeclarator<'_>,
    ctx: ExtractContext<'_>,
    module_env: &mut BTreeMap<String, ModuleEnvVal>,
    local_callables: &mut BTreeSet<String>,
) {
    let BindingPattern::BindingIdentifier(ident) = &decl.id else {
        return;
    };
    let Some(init) = &decl.init else {
        return;
    };
    let name = ident.name.to_string();
    let unwrapped = unwrap_expr(init);

    if let Some(url) = eval_static_url_expr(unwrapped, ctx, module_env) {
        module_env.insert(name, ModuleEnvVal::StringLiteral(url));
        return;
    }

    if let Expression::CallExpression(call) = unwrapped
        && let Some(base_url) = detect_axios_create_base_url(call, ctx, module_env)
    {
        module_env.insert(name, ModuleEnvVal::HttpClientInstance { base_url });
        return;
    }

    match unwrap_hoc_expr(unwrapped) {
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
            local_callables.insert(name);
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let ObjectPropertyKind::ObjectProperty(kv) = prop
                    && let Some(k) = property_key_name(&kv.key)
                {
                    local_callables.insert(format!("{name}.{k}"));
                }
            }
        }
        _ => {}
    }
}

fn register_class_callable_names(
    class_name: &str,
    cls: &Class<'_>,
    local_callables: &mut BTreeSet<String>,
) {
    for elem in &cls.body.body {
        if let ClassElement::MethodDefinition(method) = elem
            && let Some(key) = property_key_name(&method.key)
            && key != "constructor"
        {
            local_callables.insert(format!("{class_name}.{key}"));
        }
    }
}

struct AnalyzerStaticContext<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    ctx: ExtractContext<'a>,
    module_env: &'a BTreeMap<String, ModuleEnvVal>,
    imports: &'a BTreeMap<String, ImportBinding>,
    local_callables: &'a BTreeSet<String>,
}

#[allow(clippy::too_many_arguments)]
fn analyze_top_level_declarator(
    decl: &VariableDeclarator<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
    local_returns: &mut BTreeMap<String, Vec<ExportReturnSummary>>,
    local_param_reads: &mut BTreeMap<String, Vec<ParamFieldRead>>,
    local_obj_methods: &mut LocalObjMethodsMap,
    local_obj_param_reads: &mut LocalObjParamReadsMap,
    pending_obj_aliases: &mut Vec<(String, String, String)>,
    pending_obj_spreads: &mut Vec<(String, String)>,
) {
    let BindingPattern::BindingIdentifier(ident) = &decl.id else {
        return;
    };
    let Some(init) = &decl.init else {
        return;
    };
    let name = ident.name.to_string();

    match unwrap_hoc_expr(unwrap_expr(init)) {
        Expression::ArrowFunctionExpression(arrow) => {
            let returns = analyze_arrow_returns(arrow, env_ctx);
            if !returns.is_empty() {
                local_returns.insert(name.clone(), returns);
            }
            let reads = analyze_arrow_param_reads(arrow, env_ctx.source, env_ctx.line_index);
            if !reads.is_empty() {
                local_param_reads.insert(name, reads);
            }
        }
        Expression::FunctionExpression(func) => {
            let returns = analyze_function_returns(func, env_ctx);
            if !returns.is_empty() {
                local_returns.insert(name.clone(), returns);
            }
            let reads = analyze_function_param_reads(func, env_ctx.source, env_ctx.line_index);
            if !reads.is_empty() {
                local_param_reads.insert(name, reads);
            }
        }
        Expression::ObjectExpression(obj) => {
            analyze_object_methods(
                &name,
                obj,
                env_ctx,
                local_obj_methods,
                local_obj_param_reads,
                pending_obj_aliases,
                pending_obj_spreads,
            );
        }
        _ => {}
    }
}

fn analyze_object_methods(
    obj_name: &str,
    obj: &ObjectExpression<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
    local_obj_methods: &mut LocalObjMethodsMap,
    local_obj_param_reads: &mut LocalObjParamReadsMap,
    pending_obj_aliases: &mut Vec<(String, String, String)>,
    pending_obj_spreads: &mut Vec<(String, String)>,
) {
    for prop_kind in &obj.properties {
        match prop_kind {
            ObjectPropertyKind::ObjectProperty(prop) => {
                let Some(key) = property_key_name(&prop.key) else {
                    continue;
                };
                match unwrap_hoc_expr(unwrap_expr(&prop.value)) {
                    Expression::ArrowFunctionExpression(arrow) => {
                        let returns = analyze_arrow_returns(arrow, env_ctx);
                        if !returns.is_empty() {
                            extend_returns(
                                local_obj_methods.entry(obj_name.to_string()).or_default(),
                                &key,
                                returns,
                            );
                        }
                        let reads =
                            analyze_arrow_param_reads(arrow, env_ctx.source, env_ctx.line_index);
                        if !reads.is_empty() {
                            extend_param_reads(
                                local_obj_param_reads
                                    .entry(obj_name.to_string())
                                    .or_default(),
                                &key,
                                reads,
                            );
                        }
                    }
                    Expression::FunctionExpression(func) => {
                        let returns = analyze_function_returns(func, env_ctx);
                        if !returns.is_empty() {
                            extend_returns(
                                local_obj_methods.entry(obj_name.to_string()).or_default(),
                                &key,
                                returns,
                            );
                        }
                        let reads =
                            analyze_function_param_reads(func, env_ctx.source, env_ctx.line_index);
                        if !reads.is_empty() {
                            extend_param_reads(
                                local_obj_param_reads
                                    .entry(obj_name.to_string())
                                    .or_default(),
                                &key,
                                reads,
                            );
                        }
                    }
                    Expression::Identifier(id) => {
                        pending_obj_aliases.push((obj_name.to_string(), key, id.name.to_string()));
                    }
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                        if let Some((root, segs)) = extract_member_chain(&prop.value)
                            && env_ctx.imports.contains_key(&root)
                        {
                            let line = env_ctx.line_index.line_number(prop.span.start);
                            extend_returns(
                                local_obj_methods.entry(obj_name.to_string()).or_default(),
                                &key,
                                vec![ExportReturnSummary::Delegated {
                                    target: DelegationTarget::Import {
                                        import_local: root,
                                        member: Some(segs.join(".")),
                                    },
                                    source_prop: None,
                                    unwrapped_json: false,
                                    return_projection: PayloadProjection::Direct {
                                        is_vue_ref: false,
                                    },
                                    field_prefix: String::new(),
                                    call_line: line,
                                    via_framework_hook: false,
                                }],
                            );
                        }
                    }
                    _ => {}
                }
            }
            ObjectPropertyKind::SpreadProperty(spread) => {
                if let Expression::Identifier(id) = unwrap_expr(&spread.argument) {
                    pending_obj_spreads.push((obj_name.to_string(), id.name.to_string()));
                }
            }
        }
    }
}

fn analyze_class_methods(
    class_name: &str,
    cls: &Class<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
    local_obj_methods: &mut LocalObjMethodsMap,
    local_obj_param_reads: &mut LocalObjParamReadsMap,
) {
    for elem in &cls.body.body {
        let ClassElement::MethodDefinition(method) = elem else {
            continue;
        };
        let Some(key) = property_key_name(&method.key) else {
            continue;
        };
        if key == "constructor" {
            continue;
        }
        let returns = analyze_function_returns(&method.value, env_ctx);
        if !returns.is_empty() {
            extend_returns(
                local_obj_methods.entry(class_name.to_string()).or_default(),
                &key,
                returns,
            );
        }
        let reads = analyze_function_param_reads(&method.value, env_ctx.source, env_ctx.line_index);
        if !reads.is_empty() {
            extend_param_reads(
                local_obj_param_reads
                    .entry(class_name.to_string())
                    .or_default(),
                &key,
                reads,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_cjs_export(
    assign: &AssignmentExpression<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
    summary: &mut ModuleSummary,
    local_obj_methods: &mut LocalObjMethodsMap,
    local_obj_param_reads: &mut LocalObjParamReadsMap,
    pending_obj_aliases: &mut Vec<(String, String, String)>,
    pending_obj_spreads: &mut Vec<(String, String)>,
    pending_named_exports: &mut Vec<(String, String)>,
    pending_cjs_default_ident: &mut Option<String>,
) {
    let AssignmentTarget::StaticMemberExpression(member) = &assign.left else {
        return;
    };

    // `module.exports = ...`
    if member.property.name == "exports"
        && matches!(unwrap_expr(&member.object), Expression::Identifier(id) if id.name == "module")
    {
        match unwrap_hoc_expr(unwrap_expr(&assign.right)) {
            Expression::ObjectExpression(obj) => {
                analyze_object_methods(
                    "__cjs_default",
                    obj,
                    env_ctx,
                    local_obj_methods,
                    local_obj_param_reads,
                    pending_obj_aliases,
                    pending_obj_spreads,
                );
                *pending_cjs_default_ident = Some("__cjs_default".to_string());
            }
            Expression::Identifier(id) => {
                *pending_cjs_default_ident = Some(id.name.to_string());
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let returns = analyze_arrow_returns(arrow, env_ctx);
                if !returns.is_empty() {
                    extend_returns(&mut summary.export_returns, "default", returns);
                }
                let reads = analyze_arrow_param_reads(arrow, env_ctx.source, env_ctx.line_index);
                if !reads.is_empty() {
                    extend_param_reads(&mut summary.export_param_reads, "default", reads);
                }
            }
            Expression::FunctionExpression(func) => {
                let returns = analyze_function_returns(func, env_ctx);
                if !returns.is_empty() {
                    extend_returns(&mut summary.export_returns, "default", returns);
                }
                let reads = analyze_function_param_reads(func, env_ctx.source, env_ctx.line_index);
                if !reads.is_empty() {
                    extend_param_reads(&mut summary.export_param_reads, "default", reads);
                }
            }
            _ => {}
        }
        return;
    }

    // `exports.foo = ...` or `module.exports.foo = ...`
    let is_exports_prop = match unwrap_expr(&member.object) {
        Expression::Identifier(id) => id.name == "exports",
        Expression::StaticMemberExpression(inner) => {
            inner.property.name == "exports"
                && matches!(unwrap_expr(&inner.object), Expression::Identifier(id) if id.name == "module")
        }
        _ => false,
    };

    if is_exports_prop {
        let exported = member.property.name.to_string();
        match unwrap_hoc_expr(unwrap_expr(&assign.right)) {
            Expression::Identifier(id) => {
                pending_named_exports.push((id.name.to_string(), exported));
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let returns = analyze_arrow_returns(arrow, env_ctx);
                if !returns.is_empty() {
                    extend_returns(&mut summary.export_returns, &exported, returns);
                }
                let reads = analyze_arrow_param_reads(arrow, env_ctx.source, env_ctx.line_index);
                if !reads.is_empty() {
                    extend_param_reads(&mut summary.export_param_reads, &exported, reads);
                }
            }
            Expression::FunctionExpression(func) => {
                let returns = analyze_function_returns(func, env_ctx);
                if !returns.is_empty() {
                    extend_returns(&mut summary.export_returns, &exported, returns);
                }
                let reads = analyze_function_param_reads(func, env_ctx.source, env_ctx.line_index);
                if !reads.is_empty() {
                    extend_param_reads(&mut summary.export_param_reads, &exported, reads);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 1a: Intra-Function Return Summary Analyzer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum LocalVal {
    Unassigned,
    VueRefEmpty,
    StringLiteral(String),
    HttpClientInstance {
        base_url: String,
    },
    ImportedRef {
        import_local: String,
        member: Option<String>,
    },
    Direct {
        endpoint: DirectEndpointInfo,
        projection: PayloadProjection,
        field_prefix: String,
        via_framework_hook: bool,
    },
    Delegated {
        target: DelegationTarget,
        source_prop: Option<String>,
        unwrapped_json: bool,
        projection: PayloadProjection,
        field_prefix: String,
        call_line: usize,
        via_framework_hook: bool,
    },
    ObjectProps(BTreeMap<String, Box<LocalVal>>),
}

impl LocalVal {
    fn into_return_summaries(self) -> Vec<ExportReturnSummary> {
        match self {
            Self::Direct {
                endpoint,
                projection,
                field_prefix,
                via_framework_hook,
            } => vec![ExportReturnSummary::Direct {
                endpoint,
                projection,
                field_prefix,
                via_framework_hook,
            }],
            Self::Delegated {
                target,
                source_prop,
                unwrapped_json,
                projection,
                field_prefix,
                call_line,
                via_framework_hook,
            } => vec![ExportReturnSummary::Delegated {
                target,
                source_prop,
                unwrapped_json,
                return_projection: projection,
                field_prefix,
                call_line,
                via_framework_hook,
            }],
            Self::ObjectProps(props) => {
                let mut out = Vec::new();
                for (prop_name, inner) in props {
                    for summary in inner.into_return_summaries() {
                        match summary {
                            ExportReturnSummary::Direct {
                                endpoint,
                                projection: PayloadProjection::Direct { is_vue_ref },
                                field_prefix,
                                via_framework_hook,
                            } => {
                                out.push(ExportReturnSummary::Direct {
                                    endpoint,
                                    projection: PayloadProjection::Property {
                                        prop: prop_name.clone(),
                                        is_vue_ref,
                                    },
                                    field_prefix,
                                    via_framework_hook,
                                });
                            }
                            ExportReturnSummary::Delegated {
                                target,
                                source_prop,
                                unwrapped_json,
                                return_projection: PayloadProjection::Direct { is_vue_ref },
                                field_prefix,
                                call_line,
                                via_framework_hook,
                            } => {
                                out.push(ExportReturnSummary::Delegated {
                                    target,
                                    source_prop,
                                    unwrapped_json,
                                    return_projection: PayloadProjection::Property {
                                        prop: prop_name.clone(),
                                        is_vue_ref,
                                    },
                                    field_prefix,
                                    call_line,
                                    via_framework_hook,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                out
            }
            _ => Vec::new(),
        }
    }

    fn with_vue_ref(self, is_vue_ref: bool) -> Self {
        match self {
            Self::Direct {
                endpoint,
                projection,
                field_prefix,
                via_framework_hook,
            } => Self::Direct {
                endpoint,
                projection: match projection {
                    PayloadProjection::Direct { .. } => PayloadProjection::Direct { is_vue_ref },
                    PayloadProjection::Property { prop, .. } => {
                        PayloadProjection::Property { prop, is_vue_ref }
                    }
                    PayloadProjection::FetchResponse => PayloadProjection::FetchResponse,
                },
                field_prefix,
                via_framework_hook,
            },
            Self::Delegated {
                target,
                source_prop,
                unwrapped_json,
                projection,
                field_prefix,
                call_line,
                via_framework_hook,
            } => Self::Delegated {
                target,
                source_prop,
                unwrapped_json,
                projection: match projection {
                    PayloadProjection::Direct { .. } => PayloadProjection::Direct { is_vue_ref },
                    PayloadProjection::Property { prop, .. } => {
                        PayloadProjection::Property { prop, is_vue_ref }
                    }
                    PayloadProjection::FetchResponse => PayloadProjection::FetchResponse,
                },
                field_prefix,
                call_line,
                via_framework_hook,
            },
            other => other,
        }
    }

    fn wrap_in_hook(self, hook: FrameworkHookKind) -> Option<Self> {
        let is_vue_ref = hook.is_vue_ref();
        match self {
            Self::Direct {
                endpoint,
                projection: PayloadProjection::Direct { .. },
                field_prefix,
                ..
            } => Some(Self::Direct {
                endpoint,
                projection: PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref,
                },
                field_prefix,
                via_framework_hook: true,
            }),
            Self::Delegated {
                target,
                source_prop,
                unwrapped_json,
                projection: PayloadProjection::Direct { .. },
                field_prefix,
                call_line,
                ..
            } => Some(Self::Delegated {
                target,
                source_prop,
                unwrapped_json,
                projection: PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref,
                },
                field_prefix,
                call_line,
                via_framework_hook: true,
            }),
            _ => None,
        }
    }
}

fn analyze_function_returns(
    func: &Function<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
) -> Vec<ExportReturnSummary> {
    let Some(body) = &func.body else {
        return Vec::new();
    };
    let mut analyzer = FunctionBodyAnalyzer::new(env_ctx);
    analyzer.bind_unassigned_params(&func.params);
    analyzer.visit_function_body(body);
    dedup_return_summaries(analyzer.returns)
}

fn analyze_arrow_returns(
    arrow: &ArrowFunctionExpression<'_>,
    env_ctx: &AnalyzerStaticContext<'_>,
) -> Vec<ExportReturnSummary> {
    let mut analyzer = FunctionBodyAnalyzer::new(env_ctx);
    analyzer.bind_unassigned_params(&arrow.params);
    match &arrow.body {
        ArrowFunctionBody::FunctionBody(body) => {
            analyzer.visit_function_body(body);
        }
        other => {
            if let Some(expr) = other.as_expression() {
                analyzer.record_return_expr(expr);
            }
        }
    }
    dedup_return_summaries(analyzer.returns)
}

struct FunctionBodyAnalyzer<'a> {
    env_ctx: &'a AnalyzerStaticContext<'a>,
    scopes: Vec<HashMap<String, LocalVal>>,
    state_setters: HashMap<String, String>,
    returns: Vec<ExportReturnSummary>,
    fn_depth: usize,
    seen_then_calls: HashSet<u32>,
}

impl<'a> FunctionBodyAnalyzer<'a> {
    fn new(env_ctx: &'a AnalyzerStaticContext<'a>) -> Self {
        let mut root_scope = HashMap::new();
        for (name, val) in env_ctx.module_env {
            let local = match val {
                ModuleEnvVal::StringLiteral(s) => LocalVal::StringLiteral(s.clone()),
                ModuleEnvVal::HttpClientInstance { base_url } => LocalVal::HttpClientInstance {
                    base_url: base_url.clone(),
                },
            };
            root_scope.insert(name.clone(), local);
        }
        for name in env_ctx.imports.keys() {
            root_scope.insert(
                name.clone(),
                LocalVal::ImportedRef {
                    import_local: name.clone(),
                    member: None,
                },
            );
        }
        Self {
            env_ctx,
            scopes: vec![root_scope, HashMap::new()],
            state_setters: HashMap::new(),
            returns: Vec::new(),
            fn_depth: 0,
            seen_then_calls: HashSet::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 2 {
            self.scopes.pop();
        }
    }

    fn bind_var(&mut self, name: impl Into<String>, val: LocalVal) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), val);
        }
    }

    fn update_or_bind_var(&mut self, name: &str, val: LocalVal) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = val;
                return;
            }
        }
        self.bind_var(name, val);
    }

    fn lookup_var(&self, name: &str) -> Option<&LocalVal> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn bind_unassigned_params(&mut self, params: &FormalParameters<'_>) {
        for param in &params.items {
            self.register_unassigned_pattern(&param.pattern);
        }
    }

    fn register_unassigned_pattern(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), LocalVal::Unassigned);
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.register_unassigned_pattern(&prop.value);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.register_unassigned_pattern(&assign.left);
            }
            BindingPattern::ArrayPattern(arr) => {
                for elem in arr.elements.iter().flatten() {
                    self.register_unassigned_pattern(elem);
                }
            }
        }
    }

    fn record_return_expr(&mut self, expr: &Expression<'_>) {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::ConditionalExpression(cond) => {
                self.record_return_expr(&cond.consequent);
                self.record_return_expr(&cond.alternate);
            }
            Expression::LogicalExpression(log)
                if matches!(
                    log.operator,
                    LogicalOperator::Or | LogicalOperator::Coalesce | LogicalOperator::And
                ) =>
            {
                self.record_return_expr(&log.left);
                self.record_return_expr(&log.right);
            }
            _ => {
                if let Some(val) = self.eval_expr(unwrapped) {
                    self.returns.extend(val.into_return_summaries());
                }
            }
        }
    }

    fn eval_url_expr(&self, expr: &Expression<'_>) -> Option<String> {
        eval_url_expr_with_lookup(
            expr,
            self.env_ctx.ctx,
            &|name| match self.lookup_var(name) {
                Some(LocalVal::StringLiteral(s)) => Some(s.clone()),
                _ => None,
            },
        )
    }

    fn classify_http_call(&self, call: &CallExpression<'_>) -> Option<CallTarget> {
        classify_http_call_with_lookup(
            call,
            self.env_ctx.ctx,
            &|name| match self.lookup_var(name) {
                Some(LocalVal::HttpClientInstance { base_url }) => Some(base_url.clone()),
                _ => None,
            },
        )
    }

    fn eval_http_call(&self, call: &CallExpression<'_>) -> Option<LocalVal> {
        let target = self.classify_http_call(call)?;
        let url_expr = call.arguments.first()?.as_expression()?;
        let url_str = self.eval_url_expr(url_expr)?;
        let (method, projection, matched) =
            resolve_http_call_target(&target, &url_str, self.env_ctx.ctx)?;
        let origin_line = self.env_ctx.line_index.line_number(call.span.start);

        Some(LocalVal::Direct {
            endpoint: DirectEndpointInfo {
                service: matched.service,
                method,
                normalized_path: matched.normalized_path,
                origin_line,
            },
            projection,
            field_prefix: String::new(),
            via_framework_hook: false,
        })
    }

    fn eval_delegated_callee(&self, callee: &Expression<'_>, span: Span) -> Option<LocalVal> {
        let call_line = self.env_ctx.line_index.line_number(span.start);
        match unwrap_expr(callee) {
            Expression::Identifier(ident) => {
                let name = ident.name.as_str();
                match self.lookup_var(name) {
                    Some(LocalVal::ImportedRef {
                        import_local,
                        member,
                    }) => {
                        return Some(LocalVal::Delegated {
                            target: DelegationTarget::Import {
                                import_local: import_local.clone(),
                                member: member.clone(),
                            },
                            source_prop: None,
                            unwrapped_json: false,
                            projection: PayloadProjection::Direct { is_vue_ref: false },
                            field_prefix: String::new(),
                            call_line,
                            via_framework_hook: false,
                        });
                    }
                    Some(LocalVal::Unassigned) => return None,
                    _ => {}
                }
                if self.env_ctx.local_callables.contains(name) {
                    return Some(LocalVal::Delegated {
                        target: DelegationTarget::SameFile {
                            local_key: name.to_string(),
                        },
                        source_prop: None,
                        unwrapped_json: false,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix: String::new(),
                        call_line,
                        via_framework_hook: false,
                    });
                }
                None
            }
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root, segs) = extract_member_chain(callee)?;
                let member_tail = segs.join(".");
                if let Some(LocalVal::ImportedRef {
                    import_local,
                    member,
                }) = self.lookup_var(&root)
                {
                    let combined = match member {
                        Some(prefix) => format!("{prefix}.{member_tail}"),
                        None => member_tail,
                    };
                    return Some(LocalVal::Delegated {
                        target: DelegationTarget::Import {
                            import_local: import_local.clone(),
                            member: Some(combined),
                        },
                        source_prop: None,
                        unwrapped_json: false,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix: String::new(),
                        call_line,
                        via_framework_hook: false,
                    });
                }
                let local_key = format!("{root}.{member_tail}");
                if self.env_ctx.local_callables.contains(&local_key) {
                    return Some(LocalVal::Delegated {
                        target: DelegationTarget::SameFile { local_key },
                        source_prop: None,
                        unwrapped_json: false,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix: String::new(),
                        call_line,
                        via_framework_hook: false,
                    });
                }
                None
            }
            _ => None,
        }
    }

    fn eval_callback_return(&mut self, expr: &Expression<'_>) -> Option<LocalVal> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::ArrowFunctionExpression(arrow) => {
                self.push_scope();
                self.bind_unassigned_params(&arrow.params);
                let result = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => self.eval_nested_body_return(body),
                    other => other.as_expression().and_then(|e| self.eval_expr(e)),
                };
                self.pop_scope();
                result
            }
            Expression::FunctionExpression(func) => {
                let body = func.body.as_deref()?;
                self.push_scope();
                self.bind_unassigned_params(&func.params);
                let result = self.eval_nested_body_return(body);
                self.pop_scope();
                result
            }
            Expression::Identifier(_)
            | Expression::StaticMemberExpression(_)
            | Expression::ChainExpression(_) => {
                self.eval_delegated_callee(unwrapped, unwrapped.span())
            }
            _ => None,
        }
    }

    fn eval_nested_body_return(&mut self, body: &FunctionBody<'_>) -> Option<LocalVal> {
        let saved_returns = std::mem::take(&mut self.returns);
        let saved_depth = self.fn_depth;
        self.fn_depth = 0;

        let mut found: Option<LocalVal> = None;
        for stmt in &body.statements {
            if let Statement::ReturnStatement(ret) = stmt {
                if let Some(arg) = &ret.argument
                    && found.is_none()
                {
                    found = self.eval_expr(arg);
                }
            } else {
                self.visit_statement(stmt);
            }
        }

        self.fn_depth = saved_depth;
        self.returns = saved_returns;
        found
    }

    fn eval_then_callback(
        &mut self,
        cb_expr: &Expression<'_>,
        receiver_val: LocalVal,
    ) -> Option<LocalVal> {
        match unwrap_expr(cb_expr) {
            Expression::Identifier(ident) => {
                if let Some(state_var) = self.state_setters.get(ident.name.as_str()).cloned() {
                    self.update_or_bind_var(&state_var, receiver_val.clone());
                }
                Some(receiver_val)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.push_scope();
                if let Some(first_param) = arrow.params.items.first() {
                    self.bind_pattern(&first_param.pattern, receiver_val);
                }
                for param in arrow.params.items.iter().skip(1) {
                    self.register_unassigned_pattern(&param.pattern);
                }
                let result = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => self.eval_nested_body_return(body),
                    other => other.as_expression().and_then(|e| self.eval_expr(e)),
                };
                self.pop_scope();
                result
            }
            Expression::FunctionExpression(func) => {
                let body = func.body.as_deref()?;
                self.push_scope();
                if let Some(first_param) = func.params.items.first() {
                    self.bind_pattern(&first_param.pattern, receiver_val);
                }
                for param in func.params.items.iter().skip(1) {
                    self.register_unassigned_pattern(&param.pattern);
                }
                let result = self.eval_nested_body_return(body);
                self.pop_scope();
                result
            }
            _ => None,
        }
    }

    fn eval_expr(&mut self, expr: &Expression<'_>) -> Option<LocalVal> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::StringLiteral(lit) => lit
                .value
                .as_str()
                .map(|s| LocalVal::StringLiteral(s.to_string())),
            Expression::StaticMemberExpression(_) if extract_env_var_name(unwrapped).is_some() => {
                let env_name = extract_env_var_name(unwrapped)?;
                self.env_ctx
                    .ctx
                    .env
                    .get(&env_name)
                    .cloned()
                    .map(LocalVal::StringLiteral)
            }
            Expression::ObjectExpression(obj) => {
                let mut props = BTreeMap::new();
                for prop_kind in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(kv) = prop_kind
                        && let Some(key) = property_key_name(&kv.key)
                        && let Some(val) = self.eval_expr(&kv.value)
                        && matches!(val, LocalVal::Direct { .. } | LocalVal::Delegated { .. })
                    {
                        props.insert(key, Box::new(val));
                    }
                }
                if props.is_empty() {
                    None
                } else {
                    Some(LocalVal::ObjectProps(props))
                }
            }
            Expression::CallExpression(call) => self.eval_call_expr(call),
            Expression::Identifier(ident) => self.lookup_var(ident.name.as_str()).cloned(),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root_name, segments) = extract_member_chain(unwrapped)?;
                let root_val = self.lookup_var(&root_name)?.clone();
                apply_member_segments_local(root_val, &segments)
            }
            Expression::LogicalExpression(log)
                if matches!(
                    log.operator,
                    LogicalOperator::Or | LogicalOperator::Coalesce
                ) =>
            {
                self.eval_expr(&log.left)
                    .or_else(|| self.eval_expr(&log.right))
            }
            Expression::ConditionalExpression(cond) => self
                .eval_expr(&cond.consequent)
                .or_else(|| self.eval_expr(&cond.alternate)),
            _ => None,
        }
    }

    fn eval_call_expr(&mut self, call: &CallExpression<'_>) -> Option<LocalVal> {
        // 1. Local `axios.create({ baseURL })`
        if let Some(base_url) = self.detect_axios_create_local(call) {
            return Some(LocalVal::HttpClientInstance { base_url });
        }

        // 2. Vue `ref(...)` / `shallowRef(...)`
        if is_vue_ref_call(call) {
            if let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
                && let Some(inner) = self.eval_expr(arg_expr)
            {
                return Some(inner.with_vue_ref(true));
            }
            return Some(LocalVal::VueRefEmpty);
        }

        // 3. Vue `computed(() => ...)`
        if is_vue_computed_call(call)
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(inner) = self.eval_callback_return(arg_expr)
        {
            return Some(inner.with_vue_ref(true));
        }

        // 4. Framework data-fetching hooks (`useQuery`, `useSWR`, `useAsyncData`)
        if let Some((hook_kind, fetcher_expr)) = extract_framework_hook_fetcher(call) {
            let inner = self.eval_callback_return(fetcher_expr)?;
            return inner.wrap_in_hook(hook_kind);
        }

        // 5. Zod `Schema.parse(data)` / `Schema.safeParse(data)`
        if extract_zod_parse_schema_name(call).is_some()
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(inner) = self.eval_expr(arg_expr)
        {
            let is_safe_parse = matches!(
                unwrap_expr(&call.callee),
                Expression::StaticMemberExpression(m)
                    if matches!(m.property.name.as_str(), "safeParse" | "safeParseAsync")
            );
            return if is_safe_parse {
                wrap_as_data_property_local(inner)
            } else {
                Some(inner)
            };
        }

        // 6. Direct HTTP calls (`fetch`, `useFetch`, `axios`, `axios.create` instance, `[[clients]]`)
        if let Some(val) = self.eval_http_call(call) {
            return Some(val);
        }

        // 7. Promise chains & `.json()`: `.json()`, `.then(cb)`, `.catch(...)`, `.finally(...)`
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) {
            let prop = member.property.name.as_str();
            if prop == "json"
                && let Some(receiver) = self.eval_expr(&member.object)
            {
                return unwrap_fetch_json_local(receiver);
            }
            if prop == "then"
                && let Some(receiver) = self.eval_expr(&member.object)
                && let Some(cb) = call.arguments.first().and_then(Argument::as_expression)
            {
                self.seen_then_calls.insert(call.span.start);
                return self.eval_then_callback(cb, receiver);
            }
            if matches!(prop, "catch" | "finally") {
                return self.eval_expr(&member.object);
            }
        }

        // 8. Calls to imported or same-file local functions / service methods
        self.eval_delegated_callee(&call.callee, call.span)
    }

    fn detect_axios_create_local(&self, call: &CallExpression<'_>) -> Option<String> {
        detect_axios_create_with_lookup(call, self.env_ctx.ctx, &|name| match self.lookup_var(name)
        {
            Some(LocalVal::StringLiteral(s)) => Some(s.clone()),
            _ => None,
        })
    }

    fn bind_pattern(&mut self, pattern: &BindingPattern<'_>, val: LocalVal) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), val);
            }
            BindingPattern::ObjectPattern(obj_pat) => {
                for prop in &obj_pat.properties {
                    let Some(key) = property_key_name(&prop.key) else {
                        continue;
                    };
                    if let Some(sub_val) = apply_member_segments_local(val.clone(), &[key]) {
                        self.bind_pattern(&prop.value, sub_val);
                    } else {
                        self.register_unassigned_pattern(&prop.value);
                    }
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.bind_pattern(&assign.left, val);
            }
            BindingPattern::ArrayPattern(_) => {}
        }
    }
}

impl<'a> Visit<'a> for FunctionBodyAnalyzer<'_> {
    fn visit_block_statement(&mut self, it: &BlockStatement<'a>) {
        self.push_scope();
        walk::walk_block_statement(self, it);
        self.pop_scope();
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.fn_depth += 1;
        self.push_scope();
        self.bind_unassigned_params(&it.params);
        walk::walk_function(self, it, flags);
        self.pop_scope();
        self.fn_depth -= 1;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.fn_depth += 1;
        self.push_scope();
        self.bind_unassigned_params(&it.params);
        walk::walk_arrow_function_expression(self, it);
        self.pop_scope();
        self.fn_depth -= 1;
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(init) = &decl.init {
            // React `const [user, setUser] = useState(null)`
            if let BindingPattern::ArrayPattern(arr) = &decl.id
                && let Expression::CallExpression(call) = unwrap_expr(init)
                && is_react_use_state_call(call)
                && let Some(Some(BindingPattern::BindingIdentifier(state_id))) =
                    arr.elements.first()
                && let Some(Some(BindingPattern::BindingIdentifier(setter_id))) =
                    arr.elements.get(1)
            {
                let state_name = state_id.name.to_string();
                let setter_name = setter_id.name.to_string();
                let initial = call
                    .arguments
                    .first()
                    .and_then(Argument::as_expression)
                    .and_then(|e| self.eval_expr(e))
                    .unwrap_or(LocalVal::Unassigned);
                self.bind_var(state_name.clone(), initial);
                self.bind_var(setter_name.clone(), LocalVal::Unassigned);
                self.state_setters.insert(setter_name, state_name);
                walk::walk_variable_declarator(self, decl);
                return;
            }

            if let Some(val) = self.eval_expr(init) {
                self.bind_pattern(&decl.id, val);
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
            && let Some(val) = self.eval_expr(&it.right)
        {
            let updated = if is_vue_ref_assign {
                val.with_vue_ref(true)
            } else {
                val
            };
            self.update_or_bind_var(&target_name, updated);
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // React `setUser(u)` call updating state variable in outer scope.
        if let Expression::Identifier(ident) = unwrap_expr(&call.callee)
            && let Some(state_var) = self.state_setters.get(ident.name.as_str()).cloned()
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(val) = self.eval_expr(arg_expr)
        {
            self.update_or_bind_var(&state_var, val);
        }

        // Standalone promise `.then(...)` (e.g. `getUser(id).then(setUser)` or `getUser(id).then(u => { user.value = u; })`).
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee)
            && member.property.name == "then"
            && self.seen_then_calls.insert(call.span.start)
            && let Some(receiver) = self.eval_expr(&member.object)
            && let Some(cb) = call.arguments.first().and_then(Argument::as_expression)
        {
            let _ = self.eval_then_callback(cb, receiver);
        }

        walk::walk_call_expression(self, call);
    }

    fn visit_return_statement(&mut self, ret: &ReturnStatement<'a>) {
        if self.fn_depth == 0
            && let Some(arg) = &ret.argument
        {
            self.record_return_expr(arg);
        }
        walk::walk_return_statement(self, ret);
    }
}

fn apply_member_segments_local(val: LocalVal, segments: &[String]) -> Option<LocalVal> {
    if segments.is_empty() {
        return Some(val);
    }
    match val {
        LocalVal::ImportedRef {
            import_local,
            member,
        } => {
            let tail = segments.join(".");
            let combined = match member {
                Some(prefix) => format!("{prefix}.{tail}"),
                None => tail,
            };
            Some(LocalVal::ImportedRef {
                import_local,
                member: Some(combined),
            })
        }
        LocalVal::ObjectProps(mut props) => {
            let first = &segments[0];
            let inner = *props.remove(first)?;
            apply_member_segments_local(inner, &segments[1..])
        }
        LocalVal::Direct {
            endpoint,
            projection,
            field_prefix,
            via_framework_hook,
        } => match projection {
            PayloadProjection::Property { prop, is_vue_ref } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                if effective.is_empty() {
                    return Some(LocalVal::Direct {
                        endpoint,
                        projection: PayloadProjection::Property {
                            prop,
                            is_vue_ref: false,
                        },
                        field_prefix,
                        via_framework_hook,
                    });
                }
                if effective[0] != prop {
                    return None;
                }
                let rest = &effective[1..];
                let rest = if is_vue_ref && rest.first().map(String::as_str) == Some("value") {
                    &rest[1..]
                } else {
                    rest
                };
                let new_prefix = join_field_path(&field_prefix, rest);
                Some(LocalVal::Direct {
                    endpoint,
                    projection: PayloadProjection::Direct {
                        is_vue_ref: is_vue_ref && effective.len() == 1,
                    },
                    field_prefix: new_prefix,
                    via_framework_hook,
                })
            }
            PayloadProjection::Direct { is_vue_ref } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                let new_prefix = join_field_path(&field_prefix, effective);
                Some(LocalVal::Direct {
                    endpoint,
                    projection: PayloadProjection::Direct { is_vue_ref: false },
                    field_prefix: new_prefix,
                    via_framework_hook,
                })
            }
            PayloadProjection::FetchResponse => None,
        },
        LocalVal::Delegated {
            target,
            source_prop,
            unwrapped_json,
            projection,
            field_prefix,
            call_line,
            via_framework_hook,
        } => match projection {
            PayloadProjection::Property { prop, is_vue_ref } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                if effective.is_empty() {
                    return Some(LocalVal::Delegated {
                        target,
                        source_prop,
                        unwrapped_json,
                        projection: PayloadProjection::Property {
                            prop,
                            is_vue_ref: false,
                        },
                        field_prefix,
                        call_line,
                        via_framework_hook,
                    });
                }
                if effective[0] != prop {
                    return None;
                }
                let rest = &effective[1..];
                let rest = if is_vue_ref && rest.first().map(String::as_str) == Some("value") {
                    &rest[1..]
                } else {
                    rest
                };
                let new_prefix = join_field_path(&field_prefix, rest);
                Some(LocalVal::Delegated {
                    target,
                    source_prop,
                    unwrapped_json,
                    projection: PayloadProjection::Direct {
                        is_vue_ref: is_vue_ref && effective.len() == 1,
                    },
                    field_prefix: new_prefix,
                    call_line,
                    via_framework_hook,
                })
            }
            PayloadProjection::Direct { is_vue_ref } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                if effective.is_empty() {
                    return Some(LocalVal::Delegated {
                        target,
                        source_prop,
                        unwrapped_json,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix,
                        call_line,
                        via_framework_hook,
                    });
                }
                // First property access on a delegated call may unwrap the callee's wrapper property
                // (e.g. `const { data } = useUser(id)` where `useUser` returned `{ data }`).
                if source_prop.is_none() && field_prefix.is_empty() {
                    let first = effective[0].clone();
                    let rest = &effective[1..];
                    Some(LocalVal::Delegated {
                        target,
                        source_prop: Some(first),
                        unwrapped_json,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix: rest.join("."),
                        call_line,
                        via_framework_hook,
                    })
                } else {
                    let new_prefix = join_field_path(&field_prefix, effective);
                    Some(LocalVal::Delegated {
                        target,
                        source_prop,
                        unwrapped_json,
                        projection: PayloadProjection::Direct { is_vue_ref: false },
                        field_prefix: new_prefix,
                        call_line,
                        via_framework_hook,
                    })
                }
            }
            PayloadProjection::FetchResponse => None,
        },
        _ => None,
    }
}

fn unwrap_fetch_json_local(val: LocalVal) -> Option<LocalVal> {
    match val {
        LocalVal::Direct {
            endpoint,
            projection: PayloadProjection::FetchResponse,
            field_prefix,
            via_framework_hook,
        } => Some(LocalVal::Direct {
            endpoint,
            projection: PayloadProjection::Direct { is_vue_ref: false },
            field_prefix,
            via_framework_hook,
        }),
        LocalVal::Delegated {
            target,
            source_prop: None,
            unwrapped_json: false,
            projection: PayloadProjection::Direct { .. } | PayloadProjection::FetchResponse,
            field_prefix,
            call_line,
            via_framework_hook,
        } => Some(LocalVal::Delegated {
            target,
            source_prop: None,
            unwrapped_json: true,
            projection: PayloadProjection::Direct { is_vue_ref: false },
            field_prefix,
            call_line,
            via_framework_hook,
        }),
        _ => None,
    }
}

fn wrap_as_data_property_local(val: LocalVal) -> Option<LocalVal> {
    match val {
        LocalVal::Direct {
            endpoint,
            projection: PayloadProjection::Direct { .. },
            field_prefix,
            via_framework_hook,
        } => Some(LocalVal::Direct {
            endpoint,
            projection: PayloadProjection::Property {
                prop: "data".to_string(),
                is_vue_ref: false,
            },
            field_prefix,
            via_framework_hook,
        }),
        LocalVal::Delegated {
            target,
            source_prop,
            unwrapped_json,
            projection: PayloadProjection::Direct { .. },
            field_prefix,
            call_line,
            via_framework_hook,
        } => Some(LocalVal::Delegated {
            target,
            source_prop,
            unwrapped_json,
            projection: PayloadProjection::Property {
                prop: "data".to_string(),
                is_vue_ref: false,
            },
            field_prefix,
            call_line,
            via_framework_hook,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pass 1b: Exported Parameter & JSX-Prop Field-Read Scanner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ParamSlot {
    Shadowed,
    Tracked {
        param_index: usize,
        path: Vec<String>,
    },
}

fn analyze_function_param_reads(
    func: &Function<'_>,
    source: &str,
    line_index: &LineIndex,
) -> Vec<ParamFieldRead> {
    let Some(body) = &func.body else {
        return Vec::new();
    };
    let mut scanner = ParamReadScanner::new(source, line_index);
    scanner.register_params(&func.params);
    scanner.visit_function_body(body);
    scanner.finish()
}

fn analyze_arrow_param_reads(
    arrow: &ArrowFunctionExpression<'_>,
    source: &str,
    line_index: &LineIndex,
) -> Vec<ParamFieldRead> {
    let mut scanner = ParamReadScanner::new(source, line_index);
    scanner.register_params(&arrow.params);
    scanner.visit_arrow_function_body(&arrow.body);
    scanner.finish()
}

struct ParamReadScanner<'a> {
    source: &'a str,
    line_index: &'a LineIndex,
    scopes: Vec<HashMap<String, ParamSlot>>,
    reads: Vec<ParamFieldRead>,
    seen_member_starts: HashSet<u32>,
    seen_reads: HashSet<(usize, Option<String>, String, usize)>,
}

impl<'a> ParamReadScanner<'a> {
    fn new(source: &'a str, line_index: &'a LineIndex) -> Self {
        Self {
            source,
            line_index,
            scopes: vec![HashMap::new()],
            reads: Vec::new(),
            seen_member_starts: HashSet::new(),
            seen_reads: HashSet::new(),
        }
    }

    fn finish(mut self) -> Vec<ParamFieldRead> {
        self.reads.sort();
        self.reads.dedup();
        self.reads
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn bind(&mut self, name: impl Into<String>, slot: ParamSlot) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), slot);
        }
    }

    fn lookup(&self, name: &str) -> Option<(usize, Vec<String>)> {
        for scope in self.scopes.iter().rev() {
            if let Some(slot) = scope.get(name) {
                return match slot {
                    ParamSlot::Tracked { param_index, path } => Some((*param_index, path.clone())),
                    ParamSlot::Shadowed => None,
                };
            }
        }
        None
    }

    fn register_params(&mut self, params: &FormalParameters<'_>) {
        for (idx, param) in params.items.iter().enumerate() {
            self.register_param_pattern(&param.pattern, idx, &[]);
        }
    }

    fn register_shadowed_params(&mut self, params: &FormalParameters<'_>) {
        for param in &params.items {
            self.shadow_pattern(&param.pattern);
        }
    }

    fn shadow_pattern(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind(ident.name.as_str(), ParamSlot::Shadowed);
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.shadow_pattern(&prop.value);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.shadow_pattern(&assign.left);
            }
            BindingPattern::ArrayPattern(arr) => {
                for elem in arr.elements.iter().flatten() {
                    self.shadow_pattern(elem);
                }
            }
        }
    }

    fn register_param_pattern(
        &mut self,
        pattern: &BindingPattern<'_>,
        param_index: usize,
        prefix: &[String],
    ) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind(
                    ident.name.as_str(),
                    ParamSlot::Tracked {
                        param_index,
                        path: prefix.to_vec(),
                    },
                );
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    let Some(key) = property_key_name(&prop.key) else {
                        continue;
                    };
                    let mut full_segs = prefix.to_vec();
                    full_segs.push(key);
                    self.record_path_read(param_index, &full_segs, prop.span);
                    self.register_param_pattern(&prop.value, param_index, &full_segs);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.register_param_pattern(&assign.left, param_index, prefix);
            }
            BindingPattern::ArrayPattern(_) => {}
        }
    }

    fn record_path_read(&mut self, param_index: usize, full_segs: &[String], span: Span) {
        if full_segs.is_empty() {
            return;
        }
        let line = self.line_index.line_number(span.start);
        let snippet = self.line_index.line_snippet(self.source, line).to_string();

        // 1. Regular function parameter read (`prop_name: None`).
        let direct_field = full_segs.join(".");
        if self
            .seen_reads
            .insert((param_index, None, direct_field.clone(), line))
        {
            self.reads.push(ParamFieldRead {
                param_index,
                prop_name: None,
                field_path: direct_field,
                line,
                snippet: snippet.clone(),
            });
        }

        // 2. JSX prop read (`param_index == 0`, first segment is prop name, remainder is field path).
        if param_index == 0 && full_segs.len() >= 2 {
            let prop = full_segs[0].clone();
            let field_path = full_segs[1..].join(".");
            if self
                .seen_reads
                .insert((0, Some(prop.clone()), field_path.clone(), line))
            {
                self.reads.push(ParamFieldRead {
                    param_index: 0,
                    prop_name: Some(prop),
                    field_path,
                    line,
                    snippet,
                });
            }
        }
    }

    fn resolve_param_from_expr(&self, expr: &Expression<'_>) -> Option<(usize, Vec<String>)> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::Identifier(ident) => self.lookup(ident.name.as_str()),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root, segs) = extract_member_chain(unwrapped)?;
                let (param_index, mut prefix) = self.lookup(&root)?;
                prefix.extend(segs);
                Some((param_index, prefix))
            }
            _ => None,
        }
    }

    fn inspect_member_chain(&mut self, expr: &Expression<'_>) {
        let span = expr.span();
        if !self.seen_member_starts.insert(span.start) {
            return;
        }
        if let Some((root, segs)) = extract_member_chain(expr)
            && let Some((param_index, mut full_segs)) = self.lookup(&root)
        {
            full_segs.extend(segs);
            self.record_path_read(param_index, &full_segs, span);
        }
        mark_inner_member_spans(expr, &mut self.seen_member_starts);
    }
}

impl<'a> Visit<'a> for ParamReadScanner<'_> {
    fn visit_block_statement(&mut self, it: &BlockStatement<'a>) {
        self.push_scope();
        walk::walk_block_statement(self, it);
        self.pop_scope();
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.push_scope();
        self.register_shadowed_params(&it.params);
        walk::walk_function(self, it, flags);
        self.pop_scope();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.push_scope();
        self.register_shadowed_params(&it.params);
        walk::walk_arrow_function_expression(self, it);
        self.pop_scope();
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(init) = &decl.init {
            if let Some((param_index, path)) = self.resolve_param_from_expr(init) {
                let unwrapped_init = unwrap_expr(init);
                if matches!(
                    unwrapped_init,
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
                ) {
                    self.record_path_read(param_index, &path, unwrapped_init.span());
                    self.seen_member_starts.insert(unwrapped_init.span().start);
                    mark_inner_member_spans(unwrapped_init, &mut self.seen_member_starts);
                }
                self.register_param_pattern(&decl.id, param_index, &path);
            } else {
                self.shadow_pattern(&decl.id);
            }
        } else {
            self.shadow_pattern(&decl.id);
        }
        walk::walk_variable_declarator(self, decl);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // Avoid treating method invocations like `u.segundoApellido.trim()` as reading `.trim`.
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) {
            let obj = unwrap_expr(&member.object);
            if matches!(
                obj,
                Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
            ) {
                self.inspect_member_chain(obj);
            } else {
                self.seen_member_starts.insert(member.span.start);
            }
        }
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

// ---------------------------------------------------------------------------
// Pass 2: Bounded Fixed-Point Propagation (Max 6 Hops, Cycle-Safe)
// ---------------------------------------------------------------------------

// Propaga hasta seis saltos y reserva una pasada de verificación para detectar trabajo pendiente.
fn propagate_summaries(
    summaries: &BTreeMap<String, ModuleSummary>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
) -> PropagatedSummaries {
    let mut resolved_exports: EndpointMap = BTreeMap::new();
    let mut resolved_internal: EndpointMap = BTreeMap::new();
    let mut resolved_param_reads: ParamReadMap = BTreeMap::new();

    // Seed with direct HTTP endpoints and direct parameter/prop reads.
    for (file, summary) in summaries {
        for (export_key, ret_list) in &summary.export_returns {
            for ret in ret_list {
                if let ExportReturnSummary::Direct {
                    endpoint,
                    projection,
                    field_prefix,
                    via_framework_hook,
                } = ret
                {
                    let resolved = make_direct_resolved_endpoint(
                        file,
                        endpoint,
                        projection.clone(),
                        field_prefix.clone(),
                        *via_framework_hook,
                    );
                    insert_or_improve_endpoint(
                        &mut resolved_exports,
                        (file.clone(), export_key.clone()),
                        resolved,
                    );
                }
            }
        }

        for (local_key, ret_list) in &summary.internal_returns {
            for ret in ret_list {
                if let ExportReturnSummary::Direct {
                    endpoint,
                    projection,
                    field_prefix,
                    via_framework_hook,
                } = ret
                {
                    let resolved = make_direct_resolved_endpoint(
                        file,
                        endpoint,
                        projection.clone(),
                        field_prefix.clone(),
                        *via_framework_hook,
                    );
                    insert_or_improve_endpoint(
                        &mut resolved_internal,
                        (file.clone(), local_key.clone()),
                        resolved,
                    );
                }
            }
        }

        for (export_key, reads) in &summary.export_param_reads {
            let entry = resolved_param_reads
                .entry((file.clone(), export_key.clone()))
                .or_default();
            for r in reads {
                insert_unique_param_read(
                    entry,
                    ResolvedParamRead {
                        def_file: file.clone(),
                        param_index: r.param_index,
                        prop_name: r.prop_name.clone(),
                        field_path: r.field_path.clone(),
                        line: r.line,
                        snippet: r.snippet.clone(),
                    },
                );
            }
        }
    }

    let mut limited_files = BTreeSet::new();
    for hop in 0..=MAX_PROPAGATION_HOPS {
        let verification = (hop == MAX_PROPAGATION_HOPS).then(|| {
            (
                resolved_exports.clone(),
                resolved_internal.clone(),
                resolved_param_reads.clone(),
            )
        });
        let mut changed_files = BTreeSet::new();

        // 2a. Propagate barrel re-exports (`export * from`, `export { ... } from`, `export * as ns from`).
        let export_snapshot = resolved_exports.clone();
        let param_snapshot = resolved_param_reads.clone();

        for (file, summary) in summaries {
            // Star re-exports: `export * from './userService'`
            for specifier in &summary.star_reexports {
                let Some(target_file) = resolve_import(file, specifier) else {
                    continue;
                };
                if target_file == *file {
                    continue;
                }
                for ((src_file, src_key), endpoints) in &export_snapshot {
                    if *src_file != target_file || is_default_export_key(src_key) {
                        continue;
                    }
                    for ep in endpoints {
                        if ep.hops >= MAX_PROPAGATION_HOPS {
                            limited_files.insert(file.clone());
                            continue;
                        }
                        let mut next = ep.clone();
                        next.hops += 1;
                        let step = format!("re-export `{src_key}` @ {file}");
                        if !next.trace_steps.contains(&step) {
                            next.trace_steps.push(step);
                        }
                        if insert_or_improve_endpoint(
                            &mut resolved_exports,
                            (file.clone(), src_key.clone()),
                            next,
                        ) {
                            changed_files.insert(file.clone());
                        }
                    }
                }
                for ((src_file, src_key), reads) in &param_snapshot {
                    if *src_file != target_file || is_default_export_key(src_key) {
                        continue;
                    }
                    let entry = resolved_param_reads
                        .entry((file.clone(), src_key.clone()))
                        .or_default();
                    for r in reads {
                        if insert_unique_param_read(entry, r.clone()) {
                            changed_files.insert(file.clone());
                        }
                    }
                }
            }

            // Named & namespace re-exports: `export { a as b } from './x'`, `export * as ns from './x'`
            for (exported_name, target) in &summary.reexports {
                match target {
                    ReExportTarget::Named {
                        specifier,
                        imported,
                    } => {
                        let Some(target_file) = resolve_import(file, specifier) else {
                            continue;
                        };
                        if target_file == *file && imported == exported_name {
                            continue;
                        }
                        let imported_prefix = format!("{imported}.");
                        for ((src_file, src_key), endpoints) in &export_snapshot {
                            if *src_file != target_file {
                                continue;
                            }
                            let mapped_key = if src_key == imported {
                                Some(exported_name.clone())
                            } else {
                                src_key
                                    .strip_prefix(&imported_prefix)
                                    .map(|rest| format!("{exported_name}.{rest}"))
                            };
                            let Some(dest_key) = mapped_key else {
                                continue;
                            };
                            for ep in endpoints {
                                if ep.hops >= MAX_PROPAGATION_HOPS {
                                    limited_files.insert(file.clone());
                                    continue;
                                }
                                let mut next = ep.clone();
                                next.hops += 1;
                                let step = format!("re-export `{dest_key}` @ {file}");
                                if !next.trace_steps.contains(&step) {
                                    next.trace_steps.push(step);
                                }
                                if insert_or_improve_endpoint(
                                    &mut resolved_exports,
                                    (file.clone(), dest_key.clone()),
                                    next,
                                ) {
                                    changed_files.insert(file.clone());
                                }
                            }
                        }

                        for ((src_file, src_key), reads) in &param_snapshot {
                            if *src_file != target_file {
                                continue;
                            }
                            let mapped_key = if src_key == imported {
                                Some(exported_name.clone())
                            } else {
                                src_key
                                    .strip_prefix(&imported_prefix)
                                    .map(|rest| format!("{exported_name}.{rest}"))
                            };
                            let Some(dest_key) = mapped_key else {
                                continue;
                            };
                            let entry = resolved_param_reads
                                .entry((file.clone(), dest_key))
                                .or_default();
                            for r in reads {
                                if insert_unique_param_read(entry, r.clone()) {
                                    changed_files.insert(file.clone());
                                }
                            }
                        }
                    }
                    ReExportTarget::Namespace { specifier } => {
                        let Some(target_file) = resolve_import(file, specifier) else {
                            continue;
                        };
                        if target_file == *file {
                            continue;
                        }
                        for ((src_file, src_key), endpoints) in &export_snapshot {
                            if *src_file != target_file || is_default_export_key(src_key) {
                                continue;
                            }
                            let dest_key = format!("{exported_name}.{src_key}");
                            for ep in endpoints {
                                if ep.hops >= MAX_PROPAGATION_HOPS {
                                    limited_files.insert(file.clone());
                                    continue;
                                }
                                let mut next = ep.clone();
                                next.hops += 1;
                                let step = format!("re-export `{dest_key}` @ {file}");
                                if !next.trace_steps.contains(&step) {
                                    next.trace_steps.push(step);
                                }
                                if insert_or_improve_endpoint(
                                    &mut resolved_exports,
                                    (file.clone(), dest_key.clone()),
                                    next,
                                ) {
                                    changed_files.insert(file.clone());
                                }
                            }
                        }
                        for ((src_file, src_key), reads) in &param_snapshot {
                            if *src_file != target_file || is_default_export_key(src_key) {
                                continue;
                            }
                            let dest_key = format!("{exported_name}.{src_key}");
                            let entry = resolved_param_reads
                                .entry((file.clone(), dest_key))
                                .or_default();
                            for r in reads {
                                if insert_unique_param_read(entry, r.clone()) {
                                    changed_files.insert(file.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2b. Propagate `Delegated` return summaries (both internal and exported).
        let exports_snap_2 = resolved_exports.clone();
        let internal_snap_2 = resolved_internal.clone();

        for (file, summary) in summaries {
            for (local_key, ret_list) in &summary.internal_returns {
                for ret in ret_list {
                    let ExportReturnSummary::Delegated { .. } = ret else {
                        continue;
                    };
                    let propagated_eps = resolve_delegation_summary(
                        file,
                        summary,
                        ret,
                        &exports_snap_2,
                        &internal_snap_2,
                        resolve_import,
                        &mut limited_files,
                    );
                    for ep in propagated_eps {
                        if insert_or_improve_endpoint(
                            &mut resolved_internal,
                            (file.clone(), local_key.clone()),
                            ep,
                        ) {
                            changed_files.insert(file.clone());
                        }
                    }
                }
            }

            for (export_key, ret_list) in &summary.export_returns {
                for ret in ret_list {
                    let ExportReturnSummary::Delegated { .. } = ret else {
                        continue;
                    };
                    let propagated_eps = resolve_delegation_summary(
                        file,
                        summary,
                        ret,
                        &exports_snap_2,
                        &internal_snap_2,
                        resolve_import,
                        &mut limited_files,
                    );
                    for ep in propagated_eps {
                        if insert_or_improve_endpoint(
                            &mut resolved_exports,
                            (file.clone(), export_key.clone()),
                            ep,
                        ) {
                            changed_files.insert(file.clone());
                        }
                    }
                }
            }
        }

        if let Some((exports, internal, params)) = verification {
            limited_files.extend(changed_files);
            (resolved_exports, resolved_internal, resolved_param_reads) =
                (exports, internal, params);
            break;
        }
        if changed_files.is_empty() {
            break;
        }
    }

    PropagatedSummaries {
        resolved_exports,
        resolved_internal,
        resolved_param_reads,
        limited_files,
    }
}

fn make_direct_resolved_endpoint(
    file: &str,
    endpoint: &DirectEndpointInfo,
    projection: PayloadProjection,
    field_prefix: String,
    via_framework_hook: bool,
) -> ResolvedExportEndpoint {
    let confidence = if via_framework_hook {
        Confidence::Likely
    } else {
        Confidence::Confirmed
    };
    ResolvedExportEndpoint {
        service: endpoint.service.clone(),
        method: endpoint.method,
        normalized_path: endpoint.normalized_path.clone(),
        origin_file: file.to_string(),
        origin_line: endpoint.origin_line,
        confidence,
        projection,
        field_prefix,
        trace_steps: vec![
            format!("{} {}", endpoint.method.as_str(), endpoint.normalized_path),
            format!("HTTP call @ {file}:{}", endpoint.origin_line),
        ],
        hops: 0,
    }
}

// Resuelve retornos delegados y registra cuando el siguiente salto excede el límite.
fn resolve_delegation_summary(
    file: &str,
    summary: &ModuleSummary,
    ret: &ExportReturnSummary,
    resolved_exports: &EndpointMap,
    resolved_internal: &EndpointMap,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
    limited_files: &mut BTreeSet<String>,
) -> Vec<ResolvedExportEndpoint> {
    let ExportReturnSummary::Delegated {
        target,
        source_prop,
        unwrapped_json,
        return_projection,
        field_prefix,
        call_line,
        via_framework_hook,
    } = ret
    else {
        return Vec::new();
    };

    let (candidates, step_label): (Vec<ResolvedExportEndpoint>, String) = match target {
        DelegationTarget::Import {
            import_local,
            member,
        } => {
            let Some((target_file, target_key)) = resolve_imported_symbol(
                file,
                &summary.imports,
                import_local,
                member.as_deref(),
                resolve_import,
            ) else {
                return Vec::new();
            };
            let eps = resolved_exports
                .get(&(target_file, target_key.clone()))
                .cloned()
                .unwrap_or_default();
            (eps, format!("export `{target_key}` -> {file}:{call_line}"))
        }
        DelegationTarget::SameFile { local_key } => {
            let mut eps = resolved_internal
                .get(&(file.to_string(), local_key.clone()))
                .cloned()
                .unwrap_or_default();
            if let Some(exp) = resolved_exports.get(&(file.to_string(), local_key.clone())) {
                for ep in exp {
                    if !eps.contains(ep) {
                        eps.push(ep.clone());
                    }
                }
            }
            (eps, format!("call `{local_key}` -> {file}:{call_line}"))
        }
    };

    let mut out = Vec::new();
    for callee_ep in candidates {
        let Some((new_projection, new_prefix)) = compose_delegated_projection(
            &callee_ep.projection,
            &callee_ep.field_prefix,
            source_prop.as_deref(),
            *unwrapped_json,
            return_projection,
            field_prefix,
            *via_framework_hook,
        ) else {
            continue;
        };

        if callee_ep.hops >= MAX_PROPAGATION_HOPS {
            limited_files.insert(file.to_owned());
            continue;
        }

        let new_confidence = if *via_framework_hook {
            callee_ep.confidence.min(Confidence::Likely)
        } else {
            callee_ep.confidence
        };

        let mut trace_steps = callee_ep.trace_steps.clone();
        if !trace_steps.contains(&step_label) {
            trace_steps.push(step_label.clone());
        }

        out.push(ResolvedExportEndpoint {
            service: callee_ep.service,
            method: callee_ep.method,
            normalized_path: callee_ep.normalized_path,
            origin_file: callee_ep.origin_file,
            origin_line: callee_ep.origin_line,
            confidence: new_confidence,
            projection: new_projection,
            field_prefix: new_prefix,
            trace_steps,
            hops: callee_ep.hops + 1,
        });
    }

    out
}

fn compose_delegated_projection(
    callee_proj: &PayloadProjection,
    callee_prefix: &str,
    source_prop: Option<&str>,
    unwrapped_json: bool,
    return_proj: &PayloadProjection,
    deleg_prefix: &str,
    via_framework_hook: bool,
) -> Option<(PayloadProjection, String)> {
    match callee_proj {
        PayloadProjection::Direct {
            is_vue_ref: callee_ref,
        } => {
            if unwrapped_json {
                return None;
            }
            let combined_prefix = match source_prop {
                Some(prop) => {
                    join_field_path_str(&join_field_path_str(callee_prefix, prop), deleg_prefix)
                }
                None => join_field_path_str(callee_prefix, deleg_prefix),
            };
            let proj = match return_proj {
                PayloadProjection::Direct { is_vue_ref } => PayloadProjection::Direct {
                    is_vue_ref: *is_vue_ref || (*callee_ref && source_prop.is_none()),
                },
                PayloadProjection::Property { prop, is_vue_ref } => PayloadProjection::Property {
                    prop: prop.clone(),
                    is_vue_ref: *is_vue_ref || (*callee_ref && source_prop.is_none()),
                },
                PayloadProjection::FetchResponse => return None,
            };
            Some((proj, combined_prefix))
        }
        PayloadProjection::Property {
            prop: callee_prop,
            is_vue_ref: callee_ref,
        } => {
            if unwrapped_json {
                return None;
            }
            if source_prop == Some(callee_prop.as_str()) {
                let combined_prefix = join_field_path_str(callee_prefix, deleg_prefix);
                let proj = match return_proj {
                    PayloadProjection::Direct { is_vue_ref } => PayloadProjection::Direct {
                        is_vue_ref: *is_vue_ref || (*callee_ref && deleg_prefix.is_empty()),
                    },
                    PayloadProjection::Property { prop, is_vue_ref } => {
                        PayloadProjection::Property {
                            prop: prop.clone(),
                            is_vue_ref: *is_vue_ref || (*callee_ref && deleg_prefix.is_empty()),
                        }
                    }
                    PayloadProjection::FetchResponse => return None,
                };
                Some((proj, combined_prefix))
            } else if source_prop.is_none()
                && deleg_prefix.is_empty()
                && !via_framework_hook
                && matches!(return_proj, PayloadProjection::Direct { is_vue_ref: false })
            {
                // Transparent pass-through wrapper (`export const useProfile = (id) => useUser(id)`).
                Some((callee_proj.clone(), callee_prefix.to_string()))
            } else {
                None
            }
        }
        PayloadProjection::FetchResponse => {
            if unwrapped_json {
                let combined_prefix = join_field_path_str(callee_prefix, deleg_prefix);
                Some((return_proj.clone(), combined_prefix))
            } else if source_prop.is_none()
                && deleg_prefix.is_empty()
                && !via_framework_hook
                && matches!(return_proj, PayloadProjection::Direct { is_vue_ref: false })
            {
                Some((PayloadProjection::FetchResponse, callee_prefix.to_string()))
            } else {
                None
            }
        }
    }
}

fn insert_or_improve_endpoint(
    map: &mut EndpointMap,
    key: (String, String),
    candidate: ResolvedExportEndpoint,
) -> bool {
    let entry = map.entry(key).or_default();
    if let Some(existing) = entry.iter_mut().find(|ep| {
        ep.service == candidate.service
            && ep.method == candidate.method
            && ep.normalized_path == candidate.normalized_path
            && ep.origin_file == candidate.origin_file
            && ep.origin_line == candidate.origin_line
            && ep.projection == candidate.projection
            && ep.field_prefix == candidate.field_prefix
    }) {
        let is_better = candidate.confidence > existing.confidence
            || (candidate.confidence == existing.confidence && candidate.hops < existing.hops);
        if is_better {
            *existing = candidate;
            return true;
        }
        return false;
    }

    entry.push(candidate);
    entry.sort();
    true
}

fn insert_unique_param_read(list: &mut Vec<ResolvedParamRead>, item: ResolvedParamRead) -> bool {
    if list.contains(&item) {
        return false;
    }
    list.push(item);
    list.sort();
    true
}

fn is_default_export_key(key: &str) -> bool {
    key == "default" || key.starts_with("default.")
}

fn resolve_imported_symbol(
    from_file: &str,
    imports: &BTreeMap<String, ImportBinding>,
    import_local: &str,
    member: Option<&str>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<(String, String)> {
    let binding = imports.get(import_local)?;
    match binding {
        ImportBinding::Named {
            specifier,
            imported,
        } => {
            let target_file = resolve_import(from_file, specifier)?;
            let export_key = match member {
                Some(m) if !m.is_empty() => format!("{imported}.{m}"),
                _ => imported.clone(),
            };
            Some((target_file, export_key))
        }
        ImportBinding::Namespace { specifier } => {
            let target_file = resolve_import(from_file, specifier)?;
            let m = member?;
            if m.is_empty() {
                None
            } else {
                Some((target_file, m.to_string()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 3: Consumer-File Reads, JSX Props, Vue Templates & Cross-File Helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedOrigin {
    service: String,
    method: HttpMethod,
    normalized_path: String,
    origin_file: String,
    origin_line: usize,
    confidence: Confidence,
    field_prefix: String,
    trace: Vec<String>,
}

#[derive(Debug, Clone)]
enum ConsumerBinding {
    Unassigned,
    VueRefEmpty,
    StringLiteral(String),
    HttpClientInstance {
        base_url: String,
    },
    ImportedRef {
        import_local: String,
        member: Option<String>,
    },
    FetchResponse {
        origins: Vec<TrackedOrigin>,
        is_vue_ref: bool,
    },
    Wrapper {
        prop: String,
        is_vue_ref: bool,
        origins: Vec<TrackedOrigin>,
    },
    Payload {
        origins: Vec<TrackedOrigin>,
        is_vue_ref: bool,
    },
}

impl ConsumerBinding {
    fn is_template_trackable(&self) -> bool {
        matches!(self, Self::Payload { .. } | Self::Wrapper { .. })
    }

    fn with_vue_ref(self, is_vue_ref: bool) -> Self {
        match self {
            Self::FetchResponse { origins, .. } => Self::FetchResponse {
                origins,
                is_vue_ref,
            },
            Self::Wrapper { prop, origins, .. } => Self::Wrapper {
                prop,
                is_vue_ref,
                origins,
            },
            Self::Payload { origins, .. } => Self::Payload {
                origins,
                is_vue_ref,
            },
            other => other,
        }
    }

    fn wrap_in_hook(self, hook: FrameworkHookKind) -> Option<Self> {
        let is_vue_ref = hook.is_vue_ref();
        match self {
            Self::Payload { mut origins, .. } => {
                for origin in &mut origins {
                    origin.confidence = origin.confidence.min(Confidence::Likely);
                }
                Some(Self::Wrapper {
                    prop: "data".to_string(),
                    is_vue_ref,
                    origins,
                })
            }
            _ => None,
        }
    }
}

fn collect_consumer_reads(
    file_path: &str,
    source: &str,
    ctx: ExtractContext<'_>,
    summary: &ModuleSummary,
    propagated: &PropagatedSummaries,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
    out: &mut Vec<CrossFileFieldRead>,
) {
    let blocks = extract_parse_blocks(file_path, source);
    let mut template_bindings: BTreeMap<String, ConsumerBinding> = BTreeMap::new();

    for block in blocks {
        let allocator = Allocator::default();
        let Some(program) = parse_program(&allocator, &block.code, block.source_type) else {
            continue;
        };

        let mut visitor = FileCrossConsumerVisitor::new(
            file_path,
            &block.code,
            ctx,
            summary,
            propagated,
            resolve_import,
            out,
        );
        visitor.visit_program(&program);

        if block.is_vue_setup {
            let top_bindings = visitor.take_top_bindings();
            for (name, binding) in top_bindings {
                if binding.is_template_trackable() {
                    template_bindings.insert(name, binding);
                }
            }
        }
    }

    if file_path.to_ascii_lowercase().ends_with(".vue") && !template_bindings.is_empty() {
        scan_vue_template_cross_reads(file_path, source, &template_bindings, out);
    }
}

struct FileCrossConsumerVisitor<'a> {
    file_path: &'a str,
    source: &'a str,
    line_index: LineIndex,
    ctx: ExtractContext<'a>,
    summary: &'a ModuleSummary,
    propagated: &'a PropagatedSummaries,
    resolve_import: &'a dyn Fn(&str, &str) -> Option<String>,
    scopes: Vec<HashMap<String, ConsumerBinding>>,
    state_setters: HashMap<String, String>,
    seen_member_starts: HashSet<u32>,
    seen_then_calls: HashSet<u32>,
    out: &'a mut Vec<CrossFileFieldRead>,
}

impl<'a> FileCrossConsumerVisitor<'a> {
    fn new(
        file_path: &'a str,
        source: &'a str,
        ctx: ExtractContext<'a>,
        summary: &'a ModuleSummary,
        propagated: &'a PropagatedSummaries,
        resolve_import: &'a dyn Fn(&str, &str) -> Option<String>,
        out: &'a mut Vec<CrossFileFieldRead>,
    ) -> Self {
        let mut root_scope = HashMap::new();
        for name in summary.imports.keys() {
            root_scope.insert(
                name.clone(),
                ConsumerBinding::ImportedRef {
                    import_local: name.clone(),
                    member: None,
                },
            );
        }
        Self {
            file_path,
            source,
            line_index: LineIndex::new(source),
            ctx,
            summary,
            propagated,
            resolve_import,
            scopes: vec![root_scope],
            state_setters: HashMap::new(),
            seen_member_starts: HashSet::new(),
            seen_then_calls: HashSet::new(),
            out,
        }
    }

    fn take_top_bindings(mut self) -> HashMap<String, ConsumerBinding> {
        self.scopes.drain(..).next().unwrap_or_default()
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn bind_var(&mut self, name: impl Into<String>, binding: ConsumerBinding) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), binding);
        }
    }

    fn update_or_bind_var(&mut self, name: &str, binding: ConsumerBinding) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = binding;
                return;
            }
        }
        self.bind_var(name, binding);
    }

    fn lookup_var(&self, name: &str) -> Option<&ConsumerBinding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn bind_unassigned_params(&mut self, params: &FormalParameters<'_>) {
        for param in &params.items {
            self.register_unassigned_pattern(&param.pattern);
        }
    }

    fn register_unassigned_pattern(&mut self, pattern: &BindingPattern<'_>) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), ConsumerBinding::Unassigned);
            }
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    self.register_unassigned_pattern(&prop.value);
                }
            }
            BindingPattern::AssignmentPattern(assign) => {
                self.register_unassigned_pattern(&assign.left);
            }
            BindingPattern::ArrayPattern(arr) => {
                for elem in arr.elements.iter().flatten() {
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

    fn emit_read_in_current_file(
        &mut self,
        origins: &[TrackedOrigin],
        segments: &[String],
        span: Span,
    ) {
        if segments.is_empty() {
            return;
        }
        let (read_line, read_snippet) = self.location(span);
        for origin in origins {
            // Only emit cross-file reads (same-file reads are covered by `extractor.rs`).
            if origin.origin_file == self.file_path {
                continue;
            }
            let field_path = join_field_path(&origin.field_prefix, segments);
            if field_path.is_empty() {
                continue;
            }
            let mut trace = origin.trace.clone();
            trace.push(format!(
                "read `{field_path}` @ {}:{read_line}",
                self.file_path
            ));
            self.out.push(CrossFileFieldRead {
                service: origin.service.clone(),
                method: origin.method,
                normalized_path: origin.normalized_path.clone(),
                field_path,
                read_file: self.file_path.to_string(),
                read_line,
                read_snippet: read_snippet.clone(),
                origin_file: origin.origin_file.clone(),
                origin_line: origin.origin_line,
                confidence: origin.confidence,
                trace,
            });
        }
    }

    fn emit_downward_param_reads(
        &mut self,
        origins: &[TrackedOrigin],
        reads: &[ResolvedParamRead],
        step_label: &str,
    ) {
        for r in reads {
            for origin in origins {
                if r.def_file == origin.origin_file {
                    continue;
                }
                let field_path = join_field_path_str(&origin.field_prefix, &r.field_path);
                if field_path.is_empty() {
                    continue;
                }
                let mut trace = origin.trace.clone();
                trace.push(step_label.to_string());
                trace.push(format!("read `{field_path}` @ {}:{}", r.def_file, r.line));
                self.out.push(CrossFileFieldRead {
                    service: origin.service.clone(),
                    method: origin.method,
                    normalized_path: origin.normalized_path.clone(),
                    field_path,
                    read_file: r.def_file.clone(),
                    read_line: r.line,
                    read_snippet: r.snippet.clone(),
                    origin_file: origin.origin_file.clone(),
                    origin_line: origin.origin_line,
                    confidence: origin.confidence,
                    trace,
                });
            }
        }
    }

    fn eval_url_expr(&self, expr: &Expression<'_>) -> Option<String> {
        eval_url_expr_with_lookup(expr, self.ctx, &|name| match self.lookup_var(name) {
            Some(ConsumerBinding::StringLiteral(s)) => Some(s.clone()),
            _ => None,
        })
    }

    fn detect_axios_create(&self, call: &CallExpression<'_>) -> Option<ConsumerBinding> {
        detect_axios_create_with_lookup(call, self.ctx, &|name| match self.lookup_var(name) {
            Some(ConsumerBinding::StringLiteral(s)) => Some(s.clone()),
            _ => None,
        })
        .map(|base_url| ConsumerBinding::HttpClientInstance { base_url })
    }

    fn eval_direct_http_call(&self, call: &CallExpression<'_>) -> Option<ConsumerBinding> {
        let target =
            classify_http_call_with_lookup(call, self.ctx, &|name| match self.lookup_var(name) {
                Some(ConsumerBinding::HttpClientInstance { base_url }) => Some(base_url.clone()),
                _ => None,
            })?;
        let url_expr = call.arguments.first()?.as_expression()?;
        let url_str = self.eval_url_expr(url_expr)?;
        let (method, projection, matched) = resolve_http_call_target(&target, &url_str, self.ctx)?;
        let line = self.line_index.line_number(call.span.start);

        let origin = TrackedOrigin {
            service: matched.service,
            method,
            normalized_path: matched.normalized_path.clone(),
            origin_file: self.file_path.to_string(),
            origin_line: line,
            confidence: Confidence::Confirmed,
            field_prefix: String::new(),
            trace: vec![
                format!("{} {}", method.as_str(), matched.normalized_path),
                format!("HTTP call @ {}:{line}", self.file_path),
            ],
        };

        Some(binding_from_projection(projection, vec![origin]))
    }

    fn eval_imported_or_local_endpoint_call(
        &self,
        callee: &Expression<'_>,
        span: Span,
    ) -> Option<ConsumerBinding> {
        let call_line = self.line_index.line_number(span.start);

        // 1. Imported call (direct identifier or imported namespace / service object member).
        if let Some((target_file, target_key)) = self.resolve_callee_import(callee)
            && let Some(endpoints) = self
                .propagated
                .resolved_exports
                .get(&(target_file, target_key.clone()))
        {
            return binding_from_resolved_endpoints(
                endpoints,
                &format!("export `{target_key}` -> {}:{call_line}", self.file_path),
            );
        }

        // 2. Same-file helper function that itself returns a cross-file endpoint.
        let local_key = match unwrap_expr(callee) {
            Expression::Identifier(ident) => Some(ident.name.to_string()),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                extract_member_chain(callee).map(|(r, s)| format!("{r}.{}", s.join(".")))
            }
            _ => None,
        };
        if let Some(key) = local_key {
            let endpoints = self
                .propagated
                .resolved_internal
                .get(&(self.file_path.to_string(), key.clone()))
                .or_else(|| {
                    self.propagated
                        .resolved_exports
                        .get(&(self.file_path.to_string(), key.clone()))
                });
            if let Some(eps) = endpoints {
                return binding_from_resolved_endpoints(
                    eps,
                    &format!("call `{key}` -> {}:{call_line}", self.file_path),
                );
            }
        }

        None
    }

    fn resolve_callee_import(&self, callee: &Expression<'_>) -> Option<(String, String)> {
        match unwrap_expr(callee) {
            Expression::Identifier(ident) => {
                let Some(ConsumerBinding::ImportedRef {
                    import_local,
                    member,
                }) = self.lookup_var(ident.name.as_str())
                else {
                    return None;
                };
                resolve_imported_symbol(
                    self.file_path,
                    &self.summary.imports,
                    import_local,
                    member.as_deref(),
                    self.resolve_import,
                )
            }
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root, segs) = extract_member_chain(callee)?;
                let Some(ConsumerBinding::ImportedRef {
                    import_local,
                    member,
                }) = self.lookup_var(&root)
                else {
                    return None;
                };
                let tail = segs.join(".");
                let combined = match member {
                    Some(prefix) => format!("{prefix}.{tail}"),
                    None => tail,
                };
                resolve_imported_symbol(
                    self.file_path,
                    &self.summary.imports,
                    import_local,
                    Some(&combined),
                    self.resolve_import,
                )
            }
            _ => None,
        }
    }

    fn resolve_jsx_element_import(
        &self,
        name: &JSXElementName<'_>,
    ) -> Option<(String, String, String)> {
        match name {
            JSXElementName::IdentifierReference(ident) => {
                let local = ident.name.as_str();
                let (target_file, target_key) = resolve_imported_symbol(
                    self.file_path,
                    &self.summary.imports,
                    local,
                    None,
                    self.resolve_import,
                )?;
                Some((target_file, target_key, local.to_string()))
            }
            JSXElementName::MemberExpression(me) => {
                let (root, segs) = extract_jsx_member_chain(me)?;
                let member = segs.join(".");
                let (target_file, target_key) = resolve_imported_symbol(
                    self.file_path,
                    &self.summary.imports,
                    &root,
                    Some(&member),
                    self.resolve_import,
                )?;
                Some((target_file, target_key, format!("{root}.{member}")))
            }
            _ => None,
        }
    }

    fn eval_callback_binding(&mut self, expr: &Expression<'_>) -> Option<ConsumerBinding> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::ArrowFunctionExpression(arrow) => {
                self.push_scope();
                self.bind_unassigned_params(&arrow.params);
                let result = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => self.eval_nested_body_binding(body),
                    other => other
                        .as_expression()
                        .and_then(|e| self.resolve_binding_from_expr(e)),
                };
                self.pop_scope();
                result
            }
            Expression::FunctionExpression(func) => {
                let body = func.body.as_deref()?;
                self.push_scope();
                self.bind_unassigned_params(&func.params);
                let result = self.eval_nested_body_binding(body);
                self.pop_scope();
                result
            }
            Expression::Identifier(_)
            | Expression::StaticMemberExpression(_)
            | Expression::ChainExpression(_) => {
                self.eval_imported_or_local_endpoint_call(unwrapped, unwrapped.span())
            }
            _ => None,
        }
    }

    fn eval_nested_body_binding(&mut self, body: &FunctionBody<'_>) -> Option<ConsumerBinding> {
        let mut found = None;
        for stmt in &body.statements {
            if let Statement::ReturnStatement(ret) = stmt {
                if let Some(arg) = &ret.argument
                    && found.is_none()
                {
                    found = self.resolve_binding_from_expr(arg);
                }
            } else {
                self.visit_statement(stmt);
            }
        }
        found
    }

    fn eval_then_callback(
        &mut self,
        cb_expr: &Expression<'_>,
        receiver: ConsumerBinding,
    ) -> Option<ConsumerBinding> {
        match unwrap_expr(cb_expr) {
            Expression::Identifier(ident) => {
                if let Some(state_var) = self.state_setters.get(ident.name.as_str()).cloned() {
                    self.update_or_bind_var(&state_var, receiver.clone());
                }
                Some(receiver)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.push_scope();
                if let Some(first_param) = arrow.params.items.first() {
                    self.bind_pattern(&first_param.pattern, receiver);
                }
                for param in arrow.params.items.iter().skip(1) {
                    self.register_unassigned_pattern(&param.pattern);
                }
                let result = match &arrow.body {
                    ArrowFunctionBody::FunctionBody(body) => self.eval_nested_body_binding(body),
                    other => other
                        .as_expression()
                        .and_then(|e| self.resolve_binding_from_expr(e)),
                };
                self.pop_scope();
                result
            }
            Expression::FunctionExpression(func) => {
                let body = func.body.as_deref()?;
                self.push_scope();
                if let Some(first_param) = func.params.items.first() {
                    self.bind_pattern(&first_param.pattern, receiver);
                }
                for param in func.params.items.iter().skip(1) {
                    self.register_unassigned_pattern(&param.pattern);
                }
                let result = self.eval_nested_body_binding(body);
                self.pop_scope();
                result
            }
            _ => None,
        }
    }

    fn resolve_binding_from_expr(&mut self, expr: &Expression<'_>) -> Option<ConsumerBinding> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::StringLiteral(lit) => lit
                .value
                .as_str()
                .map(|s| ConsumerBinding::StringLiteral(s.to_string())),
            Expression::StaticMemberExpression(_) if extract_env_var_name(unwrapped).is_some() => {
                let env_name = extract_env_var_name(unwrapped)?;
                self.ctx
                    .env
                    .get(&env_name)
                    .cloned()
                    .map(ConsumerBinding::StringLiteral)
            }
            Expression::CallExpression(call) => self.resolve_call_binding(call),
            Expression::Identifier(ident) => self.lookup_var(ident.name.as_str()).cloned(),
            Expression::StaticMemberExpression(_) | Expression::ChainExpression(_) => {
                let (root_name, segments) = extract_member_chain(unwrapped)?;
                let root_binding = self.lookup_var(&root_name)?.clone();
                self.apply_member_segments_consumer(root_binding, &segments, unwrapped.span())
            }
            Expression::LogicalExpression(log)
                if matches!(
                    log.operator,
                    LogicalOperator::Or | LogicalOperator::Coalesce
                ) =>
            {
                self.resolve_binding_from_expr(&log.left)
                    .or_else(|| self.resolve_binding_from_expr(&log.right))
            }
            Expression::ConditionalExpression(cond) => self
                .resolve_binding_from_expr(&cond.consequent)
                .or_else(|| self.resolve_binding_from_expr(&cond.alternate)),
            _ => None,
        }
    }

    fn resolve_call_binding(&mut self, call: &CallExpression<'_>) -> Option<ConsumerBinding> {
        // 1. Local `axios.create({ baseURL })`
        if let Some(instance) = self.detect_axios_create(call) {
            return Some(instance);
        }

        // 2. Vue `ref(...)` / `shallowRef(...)`
        if is_vue_ref_call(call) {
            if let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
                && let Some(inner) = self.resolve_binding_from_expr(arg_expr)
            {
                return Some(inner.with_vue_ref(true));
            }
            return Some(ConsumerBinding::VueRefEmpty);
        }

        // 3. Vue `computed(() => ...)`
        if is_vue_computed_call(call)
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(inner) = self.eval_callback_binding(arg_expr)
        {
            return Some(inner.with_vue_ref(true));
        }

        // 4. Framework hooks (`useQuery`, `useSWR`, `useAsyncData`)
        if let Some((hook_kind, fetcher_expr)) = extract_framework_hook_fetcher(call) {
            let inner = self.eval_callback_binding(fetcher_expr)?;
            return inner.wrap_in_hook(hook_kind);
        }

        // 5. Zod `Schema.parse(...)` / `Schema.safeParse(...)`
        if extract_zod_parse_schema_name(call).is_some()
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(inner) = self.resolve_binding_from_expr(arg_expr)
        {
            let is_safe_parse = matches!(
                unwrap_expr(&call.callee),
                Expression::StaticMemberExpression(m)
                    if matches!(m.property.name.as_str(), "safeParse" | "safeParseAsync")
            );
            return if is_safe_parse {
                match inner {
                    ConsumerBinding::Payload { origins, .. } => Some(ConsumerBinding::Wrapper {
                        prop: "data".to_string(),
                        is_vue_ref: false,
                        origins,
                    }),
                    _ => None,
                }
            } else {
                Some(inner)
            };
        }

        // 6. Direct same-file HTTP calls (so passing result into imported `<UserCard>` or `formatUser` works).
        if let Some(binding) = self.eval_direct_http_call(call) {
            return Some(binding);
        }

        // 7. `.json()`, `.then(...)`, `.catch(...)`, `.finally(...)`
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) {
            let prop = member.property.name.as_str();
            if prop == "json"
                && let Some(ConsumerBinding::FetchResponse { origins, .. }) =
                    self.resolve_binding_from_expr(&member.object)
            {
                return Some(ConsumerBinding::Payload {
                    origins,
                    is_vue_ref: false,
                });
            }
            if prop == "then"
                && let Some(receiver) = self.resolve_binding_from_expr(&member.object)
                && let Some(cb) = call.arguments.first().and_then(Argument::as_expression)
            {
                self.seen_then_calls.insert(call.span.start);
                return self.eval_then_callback(cb, receiver);
            }
            if matches!(prop, "catch" | "finally") {
                return self.resolve_binding_from_expr(&member.object);
            }
        }

        // 8. Imported or same-file endpoint-returning function/method call
        self.eval_imported_or_local_endpoint_call(&call.callee, call.span)
    }

    fn apply_member_segments_consumer(
        &mut self,
        binding: ConsumerBinding,
        segments: &[String],
        span: Span,
    ) -> Option<ConsumerBinding> {
        if segments.is_empty() {
            return Some(binding);
        }
        match binding {
            ConsumerBinding::ImportedRef {
                import_local,
                member,
            } => {
                let tail = segments.join(".");
                let combined = match member {
                    Some(prefix) => format!("{prefix}.{tail}"),
                    None => tail,
                };
                Some(ConsumerBinding::ImportedRef {
                    import_local,
                    member: Some(combined),
                })
            }
            ConsumerBinding::FetchResponse {
                origins,
                is_vue_ref: true,
            } if segments.len() == 1 && segments[0] == "value" => {
                Some(ConsumerBinding::FetchResponse {
                    origins,
                    is_vue_ref: false,
                })
            }
            ConsumerBinding::Wrapper {
                prop,
                is_vue_ref,
                origins,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                if effective.is_empty() {
                    return Some(ConsumerBinding::Wrapper {
                        prop,
                        is_vue_ref: false,
                        origins,
                    });
                }
                if effective[0] != prop {
                    return None;
                }
                let after_prop = &effective[1..];
                if after_prop.is_empty() {
                    return Some(ConsumerBinding::Payload {
                        origins,
                        is_vue_ref,
                    });
                }
                let field_segs =
                    if is_vue_ref && after_prop.first().map(String::as_str) == Some("value") {
                        &after_prop[1..]
                    } else {
                        after_prop
                    };
                if field_segs.is_empty() {
                    return Some(ConsumerBinding::Payload {
                        origins,
                        is_vue_ref: false,
                    });
                }
                self.emit_read_in_current_file(&origins, field_segs, span);
                let updated_origins = append_prefix_to_origins(origins, field_segs);
                Some(ConsumerBinding::Payload {
                    origins: updated_origins,
                    is_vue_ref: false,
                })
            }
            ConsumerBinding::Payload {
                origins,
                is_vue_ref,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        segments
                    };
                if effective.is_empty() {
                    return Some(ConsumerBinding::Payload {
                        origins,
                        is_vue_ref: false,
                    });
                }
                self.emit_read_in_current_file(&origins, effective, span);
                let updated_origins = append_prefix_to_origins(origins, effective);
                Some(ConsumerBinding::Payload {
                    origins: updated_origins,
                    is_vue_ref: false,
                })
            }
            _ => None,
        }
    }

    fn bind_pattern(&mut self, pattern: &BindingPattern<'_>, binding: ConsumerBinding) {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                self.bind_var(ident.name.as_str(), binding);
            }
            BindingPattern::ObjectPattern(obj_pat) => {
                for prop in &obj_pat.properties {
                    let Some(key) = property_key_name(&prop.key) else {
                        continue;
                    };
                    match &binding {
                        ConsumerBinding::ImportedRef {
                            import_local,
                            member,
                        } => {
                            let combined = match member {
                                Some(prefix) => format!("{prefix}.{key}"),
                                None => key,
                            };
                            self.bind_pattern(
                                &prop.value,
                                ConsumerBinding::ImportedRef {
                                    import_local: import_local.clone(),
                                    member: Some(combined),
                                },
                            );
                        }
                        ConsumerBinding::Wrapper {
                            prop: wrapper_prop,
                            is_vue_ref,
                            origins,
                        } if key == *wrapper_prop => {
                            self.bind_pattern(
                                &prop.value,
                                ConsumerBinding::Payload {
                                    origins: origins.clone(),
                                    is_vue_ref: *is_vue_ref,
                                },
                            );
                        }
                        ConsumerBinding::Payload { origins, .. } => {
                            self.emit_read_in_current_file(
                                origins,
                                std::slice::from_ref(&key),
                                prop.span,
                            );
                            let updated = append_prefix_to_origins(
                                origins.clone(),
                                std::slice::from_ref(&key),
                            );
                            self.bind_pattern(
                                &prop.value,
                                ConsumerBinding::Payload {
                                    origins: updated,
                                    is_vue_ref: false,
                                },
                            );
                        }
                        _ => {
                            self.register_unassigned_pattern(&prop.value);
                        }
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
        if self.seen_member_starts.contains(&span.start) {
            return;
        }
        let Some((root_name, segments)) = extract_member_chain(expr) else {
            return;
        };
        self.seen_member_starts.insert(span.start);
        mark_inner_member_spans(expr, &mut self.seen_member_starts);

        let Some(root_binding) = self.lookup_var(&root_name).cloned() else {
            return;
        };

        match root_binding {
            ConsumerBinding::Payload {
                origins,
                is_vue_ref,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        &segments[..]
                    };
                if !effective.is_empty() {
                    self.emit_read_in_current_file(&origins, effective, span);
                }
            }
            ConsumerBinding::Wrapper {
                prop,
                is_vue_ref,
                origins,
            } => {
                let effective =
                    if is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        &segments[..]
                    };
                if effective.first().map(String::as_str) == Some(prop.as_str())
                    && effective.len() > 1
                {
                    let after_prop = &effective[1..];
                    let field_segs =
                        if is_vue_ref && after_prop.first().map(String::as_str) == Some("value") {
                            &after_prop[1..]
                        } else {
                            after_prop
                        };
                    if !field_segs.is_empty() {
                        self.emit_read_in_current_file(&origins, field_segs, span);
                    }
                }
            }
            _ => {}
        }
    }

    fn process_imported_helper_call(&mut self, call: &CallExpression<'_>) {
        let Some((target_file, target_key)) = self.resolve_callee_import(&call.callee) else {
            return;
        };
        let Some(param_reads) = self
            .propagated
            .resolved_param_reads
            .get(&(target_file.clone(), target_key.clone()))
            .cloned()
        else {
            return;
        };

        let call_line = self.line_index.line_number(call.span.start);
        for (arg_idx, arg) in call.arguments.iter().enumerate() {
            let Some(arg_expr) = arg.as_expression() else {
                continue;
            };
            let Some(ConsumerBinding::Payload { origins, .. }) =
                self.resolve_binding_from_expr(arg_expr)
            else {
                continue;
            };
            let matching: Vec<ResolvedParamRead> = param_reads
                .iter()
                .filter(|r| r.param_index == arg_idx && r.prop_name.is_none())
                .cloned()
                .collect();
            if !matching.is_empty() {
                let step = format!(
                    "call `{target_key}` -> {target_file} (from {}:{call_line})",
                    self.file_path
                );
                self.emit_downward_param_reads(&origins, &matching, &step);
            }
        }
    }

    fn process_jsx_opening(&mut self, opening: &JSXOpeningElement<'_>) {
        let Some((target_file, target_key, display_name)) =
            self.resolve_jsx_element_import(&opening.name)
        else {
            return;
        };
        let Some(param_reads) = self
            .propagated
            .resolved_param_reads
            .get(&(target_file, target_key))
            .cloned()
        else {
            return;
        };

        let jsx_line = self.line_index.line_number(opening.span.start);
        for attr_item in &opening.attributes {
            match attr_item {
                JSXAttributeItem::Attribute(attr) => {
                    let JSXAttributeName::Identifier(attr_id) = &attr.name else {
                        continue;
                    };
                    let prop_name = attr_id.name.as_str();
                    let Some(JSXAttributeValue::ExpressionContainer(container)) = &attr.value
                    else {
                        continue;
                    };
                    let Some(val_expr) = container.expression.as_expression() else {
                        continue;
                    };
                    let Some(ConsumerBinding::Payload { origins, .. }) =
                        self.resolve_binding_from_expr(val_expr)
                    else {
                        continue;
                    };
                    let matching: Vec<ResolvedParamRead> = param_reads
                        .iter()
                        .filter(|r| r.param_index == 0 && r.prop_name.as_deref() == Some(prop_name))
                        .cloned()
                        .collect();
                    if !matching.is_empty() {
                        let step = format!(
                            "JSX prop `{prop_name}` -> <{display_name}> @ {}:{jsx_line}",
                            self.file_path
                        );
                        self.emit_downward_param_reads(&origins, &matching, &step);
                    }
                }
                JSXAttributeItem::SpreadAttribute(spread) => {
                    let Some(ConsumerBinding::Payload { origins, .. }) =
                        self.resolve_binding_from_expr(&spread.argument)
                    else {
                        continue;
                    };
                    let matching: Vec<ResolvedParamRead> = param_reads
                        .iter()
                        .filter(|r| r.param_index == 0 && r.prop_name.is_none())
                        .cloned()
                        .collect();
                    if !matching.is_empty() {
                        let step = format!(
                            "JSX spread -> <{display_name}> @ {}:{jsx_line}",
                            self.file_path
                        );
                        self.emit_downward_param_reads(&origins, &matching, &step);
                    }
                }
            }
        }
    }
}

impl<'a> Visit<'a> for FileCrossConsumerVisitor<'_> {
    fn visit_block_statement(&mut self, it: &BlockStatement<'a>) {
        self.push_scope();
        walk::walk_block_statement(self, it);
        self.pop_scope();
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.push_scope();
        self.bind_unassigned_params(&it.params);
        walk::walk_function(self, it, flags);
        self.pop_scope();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.push_scope();
        self.bind_unassigned_params(&it.params);
        walk::walk_arrow_function_expression(self, it);
        self.pop_scope();
    }

    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let Some(init) = &decl.init {
            // React `const [user, setUser] = useState(null)`
            if let BindingPattern::ArrayPattern(arr) = &decl.id
                && let Expression::CallExpression(call) = unwrap_expr(init)
                && is_react_use_state_call(call)
                && let Some(Some(BindingPattern::BindingIdentifier(state_id))) =
                    arr.elements.first()
                && let Some(Some(BindingPattern::BindingIdentifier(setter_id))) =
                    arr.elements.get(1)
            {
                let state_name = state_id.name.to_string();
                let setter_name = setter_id.name.to_string();
                let initial = call
                    .arguments
                    .first()
                    .and_then(Argument::as_expression)
                    .and_then(|e| self.resolve_binding_from_expr(e))
                    .unwrap_or(ConsumerBinding::Unassigned);
                self.bind_var(state_name.clone(), initial);
                self.bind_var(setter_name.clone(), ConsumerBinding::Unassigned);
                self.state_setters.insert(setter_name, state_name);
                walk::walk_variable_declarator(self, decl);
                return;
            }

            if let Some(binding) = self.resolve_binding_from_expr(init) {
                self.bind_pattern(&decl.id, binding);
                if matches!(
                    unwrap_expr(init),
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
                ) {
                    self.seen_member_starts
                        .insert(unwrap_expr(init).span().start);
                    mark_inner_member_spans(init, &mut self.seen_member_starts);
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
                let updated = if is_vue_ref_assign {
                    binding.with_vue_ref(true)
                } else {
                    binding
                };
                self.update_or_bind_var(&target_name, updated);
                if matches!(
                    unwrap_expr(&it.right),
                    Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
                ) {
                    self.seen_member_starts
                        .insert(unwrap_expr(&it.right).span().start);
                    mark_inner_member_spans(&it.right, &mut self.seen_member_starts);
                }
            }
            if is_vue_ref_assign {
                self.seen_member_starts.insert(it.left.span().start);
            }
        }
        walk::walk_assignment_expression(self, it);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // React `setUser(u)` call
        if let Expression::Identifier(ident) = unwrap_expr(&call.callee)
            && let Some(state_var) = self.state_setters.get(ident.name.as_str()).cloned()
            && let Some(arg_expr) = call.arguments.first().and_then(Argument::as_expression)
            && let Some(binding) = self.resolve_binding_from_expr(arg_expr)
        {
            self.update_or_bind_var(&state_var, binding);
        }

        // Standalone `.then(...)` (e.g. `getUser(id).then(setUser)` or `getUser(id).then(u => { user.value = u; })`)
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee)
            && member.property.name == "then"
            && self.seen_then_calls.insert(call.span.start)
            && let Some(receiver) = self.resolve_binding_from_expr(&member.object)
            && let Some(cb) = call.arguments.first().and_then(Argument::as_expression)
        {
            let _ = self.eval_then_callback(cb, receiver);
        }

        // Avoid treating method invocations like `user.segundoApellido.trim()` as reading `.trim`.
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) {
            let obj = unwrap_expr(&member.object);
            if matches!(
                obj,
                Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
            ) {
                self.inspect_member_chain(obj);
            } else {
                self.seen_member_starts.insert(member.span.start);
            }
        }

        // Downward propagation into imported helpers (e.g. `formatUser(user)`).
        self.process_imported_helper_call(call);

        walk::walk_call_expression(self, call);
    }

    fn visit_jsx_opening_element(&mut self, opening: &JSXOpeningElement<'a>) {
        self.process_jsx_opening(opening);
        walk::walk_jsx_opening_element(self, opening);
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

fn binding_from_projection(
    projection: PayloadProjection,
    origins: Vec<TrackedOrigin>,
) -> ConsumerBinding {
    match projection {
        PayloadProjection::Direct { is_vue_ref } => ConsumerBinding::Payload {
            origins,
            is_vue_ref,
        },
        PayloadProjection::Property { prop, is_vue_ref } => ConsumerBinding::Wrapper {
            prop,
            is_vue_ref,
            origins,
        },
        PayloadProjection::FetchResponse => ConsumerBinding::FetchResponse {
            origins,
            is_vue_ref: false,
        },
    }
}

fn binding_from_resolved_endpoints(
    endpoints: &[ResolvedExportEndpoint],
    step_label: &str,
) -> Option<ConsumerBinding> {
    let first = endpoints.first()?;
    let projection = first.projection.clone();
    let origins: Vec<TrackedOrigin> = endpoints
        .iter()
        .filter(|ep| ep.projection == projection)
        .map(|ep| {
            let mut trace = ep.trace_steps.clone();
            if !trace.iter().any(|s| s == step_label) {
                trace.push(step_label.to_string());
            }
            TrackedOrigin {
                service: ep.service.clone(),
                method: ep.method,
                normalized_path: ep.normalized_path.clone(),
                origin_file: ep.origin_file.clone(),
                origin_line: ep.origin_line,
                confidence: ep.confidence,
                field_prefix: ep.field_prefix.clone(),
                trace,
            }
        })
        .collect();

    if origins.is_empty() {
        None
    } else {
        Some(binding_from_projection(projection, origins))
    }
}

fn append_prefix_to_origins(
    mut origins: Vec<TrackedOrigin>,
    segments: &[String],
) -> Vec<TrackedOrigin> {
    for origin in &mut origins {
        origin.field_prefix = join_field_path(&origin.field_prefix, segments);
    }
    origins
}

// ---------------------------------------------------------------------------
// Vue `<template>` Cross-File Expression Scanner
// ---------------------------------------------------------------------------

fn scan_vue_template_cross_reads(
    file_path: &str,
    source: &str,
    bindings: &BTreeMap<String, ConsumerBinding>,
    out: &mut Vec<CrossFileFieldRead>,
) {
    let template_exprs = extract_vue_template_expressions(source);
    for tpl_expr in template_exprs {
        let allocator = Allocator::default();
        let Ok(parsed_expr) =
            Parser::new(&allocator, &tpl_expr.expr, SourceType::ts()).parse_expression()
        else {
            continue;
        };

        let mut scanner = VueTemplateCrossScanner {
            file_path,
            bindings,
            line: tpl_expr.line,
            snippet: &tpl_expr.snippet,
            seen_member_starts: HashSet::new(),
            out,
        };
        scanner.visit_expression(&parsed_expr);
    }
}

struct VueTemplateCrossScanner<'a> {
    file_path: &'a str,
    bindings: &'a BTreeMap<String, ConsumerBinding>,
    line: usize,
    snippet: &'a str,
    seen_member_starts: HashSet<u32>,
    out: &'a mut Vec<CrossFileFieldRead>,
}

impl VueTemplateCrossScanner<'_> {
    fn inspect_member_chain(&mut self, expr: &Expression<'_>) {
        let span = expr.span();
        if self.seen_member_starts.contains(&span.start) {
            return;
        }
        let Some((root_name, segments)) = extract_member_chain(expr) else {
            return;
        };
        self.seen_member_starts.insert(span.start);
        mark_inner_member_spans(expr, &mut self.seen_member_starts);

        let Some(binding) = self.bindings.get(&root_name) else {
            return;
        };

        match binding {
            ConsumerBinding::Payload {
                origins,
                is_vue_ref,
            } => {
                // Vue auto-unwraps top-level refs in templates, and also tolerate explicit `.value`.
                let effective =
                    if *is_vue_ref && segments.first().map(String::as_str) == Some("value") {
                        &segments[1..]
                    } else {
                        &segments[..]
                    };
                self.emit_reads(origins, effective);
            }
            ConsumerBinding::Wrapper {
                prop,
                is_vue_ref,
                origins,
            } if segments.first().map(String::as_str) == Some(prop.as_str())
                && segments.len() > 1 =>
            {
                let after_prop = &segments[1..];
                let effective =
                    if *is_vue_ref && after_prop.first().map(String::as_str) == Some("value") {
                        &after_prop[1..]
                    } else {
                        after_prop
                    };
                self.emit_reads(origins, effective);
            }
            _ => {}
        }
    }

    fn emit_reads(&mut self, origins: &[TrackedOrigin], segments: &[String]) {
        if segments.is_empty() {
            return;
        }
        for origin in origins {
            if origin.origin_file == self.file_path {
                continue;
            }
            let field_path = join_field_path(&origin.field_prefix, segments);
            if field_path.is_empty() {
                continue;
            }
            let mut trace = origin.trace.clone();
            trace.push(format!(
                "read `{field_path}` @ {}:{}",
                self.file_path, self.line
            ));
            self.out.push(CrossFileFieldRead {
                service: origin.service.clone(),
                method: origin.method,
                normalized_path: origin.normalized_path.clone(),
                field_path,
                read_file: self.file_path.to_string(),
                read_line: self.line,
                read_snippet: self.snippet.to_string(),
                origin_file: origin.origin_file.clone(),
                origin_line: origin.origin_line,
                confidence: origin.confidence,
                trace,
            });
        }
    }
}

impl<'a> Visit<'a> for VueTemplateCrossScanner<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) {
            let obj = unwrap_expr(&member.object);
            if matches!(
                obj,
                Expression::StaticMemberExpression(_) | Expression::ChainExpression(_)
            ) {
                self.inspect_member_chain(obj);
            } else {
                self.seen_member_starts.insert(member.span.start);
            }
        }
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

// ---------------------------------------------------------------------------
// Shared AST & HTTP Classification Helpers
// ---------------------------------------------------------------------------

fn eval_static_url_expr(
    expr: &Expression<'_>,
    ctx: ExtractContext<'_>,
    module_env: &BTreeMap<String, ModuleEnvVal>,
) -> Option<String> {
    eval_url_expr_with_lookup(expr, ctx, &|name| match module_env.get(name) {
        Some(ModuleEnvVal::StringLiteral(s)) => Some(s.clone()),
        _ => None,
    })
}

fn detect_axios_create_base_url(
    call: &CallExpression<'_>,
    ctx: ExtractContext<'_>,
    module_env: &BTreeMap<String, ModuleEnvVal>,
) -> Option<String> {
    detect_axios_create_with_lookup(call, ctx, &|name| match module_env.get(name) {
        Some(ModuleEnvVal::StringLiteral(s)) => Some(s.clone()),
        _ => None,
    })
}

fn eval_url_expr_with_lookup(
    expr: &Expression<'_>,
    ctx: ExtractContext<'_>,
    lookup_str: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let unwrapped = unwrap_expr(expr);
    match unwrapped {
        Expression::StringLiteral(lit) => lit.value.as_str().map(ToOwned::to_owned),
        Expression::TemplateLiteral(tpl) => eval_template_url_with_lookup(tpl, ctx, lookup_str),
        Expression::Identifier(ident) => lookup_str(ident.name.as_str()),
        Expression::StaticMemberExpression(_) => {
            let env_name = extract_env_var_name(unwrapped)?;
            ctx.env.get(&env_name).cloned()
        }
        Expression::LogicalExpression(log)
            if matches!(
                log.operator,
                LogicalOperator::Or | LogicalOperator::Coalesce
            ) =>
        {
            eval_url_expr_with_lookup(&log.left, ctx, lookup_str)
                .or_else(|| eval_url_expr_with_lookup(&log.right, ctx, lookup_str))
        }
        Expression::BinaryExpression(bin) if bin.operator.as_str() == "+" => {
            let left = eval_url_expr_with_lookup(&bin.left, ctx, lookup_str)?;
            match eval_url_expr_with_lookup(&bin.right, ctx, lookup_str) {
                Some(right) => Some(format!("{left}{right}")),
                None if left.ends_with('/') => Some(format!("{left}:param")),
                None => None,
            }
        }
        _ => None,
    }
}

fn eval_template_url_with_lookup(
    tpl: &TemplateLiteral<'_>,
    ctx: ExtractContext<'_>,
    lookup_str: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
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
                    if let Some(val) = lookup_str(ident.name.as_str()) {
                        out.push_str(&val);
                    } else if out.is_empty() {
                        return None;
                    } else {
                        out.push(':');
                        out.push_str(ident.name.as_str());
                    }
                }
                Expression::StaticMemberExpression(_) => {
                    if let Some(env_name) = extract_env_var_name(unwrapped) {
                        if let Some(val) = ctx.env.get(&env_name) {
                            out.push_str(val);
                        } else if out.is_empty() {
                            return None;
                        } else {
                            out.push_str(":param");
                        }
                    } else if out.is_empty() {
                        return None;
                    } else {
                        out.push_str(":param");
                    }
                }
                _ => {
                    if out.is_empty() {
                        return None;
                    }
                    out.push_str(":param");
                }
            }
        }
    }
    Some(out)
}

fn detect_axios_create_with_lookup(
    call: &CallExpression<'_>,
    ctx: ExtractContext<'_>,
    lookup_str: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
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
            && let Some(base_url) = eval_url_expr_with_lookup(&kv.value, ctx, lookup_str)
        {
            return Some(base_url);
        }
    }
    None
}

// Usa la misma resolución de métodos que la extracción local y omite las llamadas indeterminadas.
fn classify_http_call_with_lookup(
    call: &CallExpression<'_>,
    ctx: ExtractContext<'_>,
    lookup_axios_base: &dyn Fn(&str) -> Option<String>,
) -> Option<CallTarget> {
    if let Some(callee_str) = format_callee_expr(unwrap_expr(&call.callee)) {
        for client in ctx.clients {
            if callee_str == client.callee {
                let method = options_http_method(call)?;
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
                    let method = options_http_method(call)?;
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
                        method,
                        is_use_fetch: false,
                    });
                }
            }
        }
    }

    match unwrap_expr(&call.callee) {
        Expression::Identifier(ident) if ident.name == "fetch" => {
            let method = options_http_method(call)?;
            Some(CallTarget::Fetch(method))
        }
        Expression::Identifier(ident)
            if ident.name == "useFetch" || ident.name == "useLazyFetch" =>
        {
            let method = options_http_method(call)?;
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
            if let Some(base_url) = lookup_axios_base(obj.name.as_str()) {
                return Some(CallTarget::AxiosInstance { base_url, method });
            }
            None
        }
        _ => None,
    }
}

fn resolve_http_call_target(
    target: &CallTarget,
    url_str: &str,
    ctx: ExtractContext<'_>,
) -> Option<(HttpMethod, PayloadProjection, ServiceEndpointMatch)> {
    match target {
        CallTarget::Fetch(method) => {
            let matched = resolve_url_against_services(url_str, ctx.services)?;
            Some((*method, PayloadProjection::FetchResponse, matched))
        }
        CallTarget::UseFetch(method) => {
            let matched = resolve_url_against_services(url_str, ctx.services)?;
            Some((
                *method,
                PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref: true,
                },
                matched,
            ))
        }
        CallTarget::Axios(method) => {
            let matched = resolve_url_against_services(url_str, ctx.services)?;
            Some((
                *method,
                PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref: false,
                },
                matched,
            ))
        }
        CallTarget::AxiosInstance { base_url, method } => {
            let full_url = if is_absolute_http_url(url_str) {
                url_str.to_string()
            } else {
                let base = base_url.trim_end_matches('/');
                let rel = url_str.trim_start_matches('/');
                format!("{base}/{rel}")
            };
            let matched = resolve_url_against_services(&full_url, ctx.services)?;
            Some((
                *method,
                PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref: false,
                },
                matched,
            ))
        }
        CallTarget::Custom {
            target_service,
            path_prefix,
            method,
            is_use_fetch,
        } => {
            let matched = if is_absolute_http_url(url_str) {
                resolve_url_against_services(url_str, ctx.services)?
            } else {
                ServiceEndpointMatch {
                    service: target_service.clone(),
                    normalized_path: normalize_relative_path(url_str, path_prefix.as_deref()),
                }
            };
            let projection = if *is_use_fetch {
                PayloadProjection::Property {
                    prop: "data".to_string(),
                    is_vue_ref: true,
                }
            } else {
                PayloadProjection::Direct { is_vue_ref: false }
            };
            Some((*method, projection, matched))
        }
    }
}

fn extract_framework_hook_fetcher<'b, 'a>(
    call: &'b CallExpression<'a>,
) -> Option<(FrameworkHookKind, &'b Expression<'a>)> {
    let hook_name = match unwrap_expr(&call.callee) {
        Expression::Identifier(id) => id.name.as_str(),
        Expression::StaticMemberExpression(member) => member.property.name.as_str(),
        _ => return None,
    };

    match hook_name {
        "useQuery" | "useSuspenseQuery" | "useInfiniteQuery" => {
            // TanStack Query v5 object form: `useQuery({ queryKey: [...], queryFn: () => ... })`
            if let Some(first) = call.arguments.first().and_then(Argument::as_expression)
                && let Expression::ObjectExpression(obj) = unwrap_expr(first)
            {
                for prop in &obj.properties {
                    if let ObjectPropertyKind::ObjectProperty(kv) = prop
                        && property_key_name(&kv.key).as_deref() == Some("queryFn")
                    {
                        return Some((FrameworkHookKind::ReactQuery, &kv.value));
                    }
                }
            }
            // React Query v3/v4 positional form: `useQuery(key, () => ...)`
            let second = call.arguments.get(1).and_then(Argument::as_expression)?;
            Some((FrameworkHookKind::ReactQuery, second))
        }
        "useSWR" | "useSWRImmutable" => {
            let second = call.arguments.get(1).and_then(Argument::as_expression)?;
            Some((FrameworkHookKind::Swr, second))
        }
        "useAsyncData" | "useLazyAsyncData" => {
            // `useAsyncData(key, () => ...)` or `useAsyncData(() => ...)`
            if let Some(second) = call.arguments.get(1).and_then(Argument::as_expression)
                && matches!(
                    unwrap_expr(second),
                    Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
                )
            {
                return Some((FrameworkHookKind::NuxtAsyncData, second));
            }
            let first = call.arguments.first().and_then(Argument::as_expression)?;
            if matches!(
                unwrap_expr(first),
                Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
            ) {
                return Some((FrameworkHookKind::NuxtAsyncData, first));
            }
            call.arguments
                .get(1)
                .and_then(Argument::as_expression)
                .map(|expr| (FrameworkHookKind::NuxtAsyncData, expr))
        }
        _ => None,
    }
}

fn unwrap_hoc_expr<'b, 'a>(mut expr: &'b Expression<'a>) -> &'b Expression<'a> {
    loop {
        match unwrap_expr(expr) {
            Expression::CallExpression(call) => {
                let callee_name = match unwrap_expr(&call.callee) {
                    Expression::Identifier(id) => Some(id.name.as_str()),
                    Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                    _ => None,
                };
                if matches!(callee_name, Some("memo" | "forwardRef" | "freeze"))
                    && let Some(first) = call.arguments.first().and_then(Argument::as_expression)
                {
                    expr = first;
                    continue;
                }
                return unwrap_expr(expr);
            }
            other => return other,
        }
    }
}

fn unwrap_expr<'b, 'a>(mut expr: &'b Expression<'a>) -> &'b Expression<'a> {
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

fn is_react_use_state_call(call: &CallExpression<'_>) -> bool {
    match unwrap_expr(&call.callee) {
        Expression::Identifier(ident) => ident.name == "useState",
        Expression::StaticMemberExpression(member) => member.property.name == "useState",
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

fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(ident) => Some(ident.name.to_string()),
        PropertyKey::StaticStringLiteral(lit) | PropertyKey::StringLiteral(lit) => {
            lit.value.as_str().map(ToOwned::to_owned)
        }
        _ => None,
    }
}

fn module_export_name_str(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(id) => Some(id.name.to_string()),
        ModuleExportName::IdentifierReference(id) => Some(id.name.to_string()),
        ModuleExportName::StringLiteral(lit) => lit.value.as_str().map(ToOwned::to_owned),
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

fn extract_jsx_member_chain(me: &JSXMemberExpression<'_>) -> Option<(String, Vec<String>)> {
    let mut segments = vec![me.property.name.to_string()];
    let mut current = &me.object;

    loop {
        match current {
            JSXMemberExpressionObject::IdentifierReference(id) => {
                segments.reverse();
                return Some((id.name.to_string(), segments));
            }
            JSXMemberExpressionObject::MemberExpression(inner) => {
                segments.push(inner.property.name.to_string());
                current = &inner.object;
            }
            JSXMemberExpressionObject::ThisExpression(_) => return None,
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

fn join_field_path_str(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        suffix.to_string()
    } else if suffix.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}.{suffix}")
    }
}

fn extend_returns(
    map: &mut BTreeMap<String, Vec<ExportReturnSummary>>,
    key: &str,
    items: Vec<ExportReturnSummary>,
) {
    let entry = map.entry(key.to_string()).or_default();
    for item in items {
        if !entry.contains(&item) {
            entry.push(item);
        }
    }
}

fn extend_param_reads(
    map: &mut BTreeMap<String, Vec<ParamFieldRead>>,
    key: &str,
    items: Vec<ParamFieldRead>,
) {
    let entry = map.entry(key.to_string()).or_default();
    for item in items {
        if !entry.contains(&item) {
            entry.push(item);
        }
    }
    entry.sort();
}

fn dedup_return_summaries(items: Vec<ExportReturnSummary>) -> Vec<ExportReturnSummary> {
    let mut out = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn dedup_and_sort_reads(reads: Vec<CrossFileFieldRead>) -> Vec<CrossFileFieldRead> {
    type DedupKey = (
        String,
        HttpMethod,
        String,
        String,
        String,
        usize,
        String,
        usize,
    );

    let mut best: BTreeMap<DedupKey, CrossFileFieldRead> = BTreeMap::new();
    for item in reads {
        let key = (
            item.service.clone(),
            item.method,
            item.normalized_path.clone(),
            item.field_path.clone(),
            item.read_file.clone(),
            item.read_line,
            item.origin_file.clone(),
            item.origin_line,
        );
        match best.get_mut(&key) {
            Some(existing) => {
                let replace = item.confidence > existing.confidence
                    || (item.confidence == existing.confidence
                        && (item.trace.len(), &item.trace)
                            < (existing.trace.len(), &existing.trace));
                if replace {
                    *existing = item;
                }
            }
            None => {
                best.insert(key, item);
            }
        }
    }

    let mut results: Vec<CrossFileFieldRead> = best.into_values().collect();
    results.sort_by(|a, b| {
        a.service
            .cmp(&b.service)
            .then_with(|| a.method.cmp(&b.method))
            .then_with(|| a.normalized_path.cmp(&b.normalized_path))
            .then_with(|| a.read_file.cmp(&b.read_file))
            .then_with(|| a.read_line.cmp(&b.read_line))
            .then_with(|| a.field_path.cmp(&b.field_path))
            .then_with(|| a.origin_file.cmp(&b.origin_file))
            .then_with(|| a.origin_line.cmp(&b.origin_line))
            .then_with(|| a.confidence.cmp(&b.confidence))
            .then_with(|| a.trace.cmp(&b.trace))
    });
    results
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use laine_core::config::{CustomClientConfig, ServiceConfig};

    use super::*;

    // Distingue una cadena que concluye en el límite de otra que necesita un salto más.
    #[test]
    fn reports_summaries_that_exceed_the_hop_limit() {
        let services = vec![laine_core::config::ServiceConfig {
            name: "api".into(),
            path: "api".into(),
            openapi: None,
            routes: None,
            base_urls: vec!["http://api".into()],
        }];
        let env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &env,
        };
        let mut files = vec![("step0.ts".into(), "export async function load() { const res = await fetch('http://api/users'); return res.json(); }".into())];
        let resolver = |_: &str, spec: &str| Some(format!("{}.ts", spec.trim_start_matches("./")));
        for hop in 1..=MAX_PROPAGATION_HOPS {
            files.push((
                format!("step{hop}.ts"),
                format!("export {{ load }} from './step{}';", hop - 1),
            ));
        }
        files.push(("read.ts".into(), format!("import {{ load }} from './step{MAX_PROPAGATION_HOPS}'; const user = await load(); console.log(user.name);")));
        let bounded = analyze_cross_file_dataflow_with_limits(&files, ctx, &resolver);
        assert!(
            bounded.limited_files.is_empty(),
            "{:?}",
            bounded.limited_files
        );
        assert!(
            bounded
                .field_reads
                .iter()
                .any(|read| read.field_path == "name")
        );

        let excessive = format!("step{}.ts", MAX_PROPAGATION_HOPS + 1);
        files.push((
            excessive.clone(),
            format!("export {{ load }} from './step{MAX_PROPAGATION_HOPS}';"),
        ));
        let result = analyze_cross_file_dataflow_with_limits(&files, ctx, &resolver);
        assert_eq!(result.limited_files, BTreeSet::from([excessive]));
        assert_eq!(result.field_reads, bounded.field_reads);
    }

    fn user_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            name: "user-service".into(),
            path: PathBuf::from("./user-service"),
            openapi: Some(PathBuf::from("openapi.yaml")),
            routes: None,
            base_urls: vec!["http://user-service".into()],
        }]
    }

    /// Minimal deterministic test module resolver matching relative imports against an in-memory file set.
    fn make_test_resolver(
        files: &[(String, String)],
    ) -> impl Fn(&str, &str) -> Option<String> + '_ {
        let known: BTreeSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        move |from_file: &str, specifier: &str| {
            if !specifier.starts_with('.') {
                return None;
            }
            let base_dir = from_file.rsplit_once('/').map_or("", |(dir, _)| dir);
            let mut parts: Vec<&str> = if base_dir.is_empty() {
                Vec::new()
            } else {
                base_dir.split('/').collect()
            };
            for seg in specifier.split('/') {
                match seg {
                    "" | "." => {}
                    ".." => {
                        parts.pop();
                    }
                    other => parts.push(other),
                }
            }
            let joined = parts.join("/");
            let candidates = [
                joined.clone(),
                format!("{joined}.ts"),
                format!("{joined}.tsx"),
                format!("{joined}.js"),
                format!("{joined}.jsx"),
                format!("{joined}.mjs"),
                format!("{joined}.cjs"),
                format!("{joined}.vue"),
                format!("{joined}/index.ts"),
                format!("{joined}/index.tsx"),
                format!("{joined}/index.js"),
                format!("{joined}/index.jsx"),
            ];
            candidates
                .into_iter()
                .find(|cand| known.contains(cand.as_str()))
        }
    }

    #[test]
    fn untyped_js_three_file_chain_with_react_query_tracks_field_read() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/services/user.js".to_string(),
                r#"export async function getUser(id) {
  const res = await fetch(`http://user-service/users/${id}`);
  return res.json();
}
"#
                .to_string(),
            ),
            (
                "src/hooks/useUser.js".to_string(),
                r#"import { useQuery } from '@tanstack/react-query';
import { getUser } from '../services/user';

export function useUser(id) {
  return useQuery({
    queryKey: ['user', id],
    queryFn: () => getUser(id),
  });
}
"#
                .to_string(),
            ),
            (
                "src/components/UserCard.jsx".to_string(),
                r#"import { useUser } from '../hooks/useUser';

export function UserCard({ id }) {
  const { data: user } = useUser(id);
  return <p>{user.segundoApellido}</p>;
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 1);
        let read = &reads[0];
        assert_eq!(read.service, "user-service");
        assert_eq!(read.method, HttpMethod::Get);
        assert_eq!(read.normalized_path, "/users/:id");
        assert_eq!(read.field_path, "segundoApellido");
        assert_eq!(read.read_file, "src/components/UserCard.jsx");
        assert_eq!(read.read_line, 5);
        assert_eq!(read.origin_file, "src/services/user.js");
        assert_eq!(read.origin_line, 2);
        assert_eq!(read.confidence, Confidence::Likely);
        assert!(read.trace.iter().any(|t| t == "GET /users/:id"));
        assert!(
            read.trace
                .iter()
                .any(|t| t.contains("src/services/user.js:2"))
        );
        assert!(
            read.trace
                .iter()
                .any(|t| t.contains("src/hooks/useUser.js"))
        );
        assert!(
            read.trace
                .iter()
                .any(|t| t.contains("read `segundoApellido` @ src/components/UserCard.jsx:5"))
        );

        // Verify order independence: reversing `files` produces identical output.
        let mut reversed = files.clone();
        reversed.reverse();
        let reads_reversed = analyze_cross_file_dataflow(&reversed, ctx, &resolver);
        assert_eq!(reads, reads_reversed);
    }

    #[test]
    fn jsx_prop_propagation_across_files_destructured_and_props_object() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/services/user.ts".to_string(),
                r#"export async function getUser(id: string) {
  const res = await fetch(`http://user-service/users/${id}`);
  return res.json();
}
"#
                .to_string(),
            ),
            (
                "src/components/UserCard.tsx".to_string(),
                r#"export function UserCard({ user }: { user: any }) {
  return <div>{user.segundoApellido}</div>;
}
"#
                .to_string(),
            ),
            (
                "src/components/UserBadge.tsx".to_string(),
                r#"export default function UserBadge(props: { profile: any }) {
  return <span>{props.profile.email}</span>;
}
"#
                .to_string(),
            ),
            (
                "src/pages/Profile.tsx".to_string(),
                r#"import { getUser } from '../services/user';
import { UserCard } from '../components/UserCard';
import UserBadge from '../components/UserBadge';

export async function Profile({ id }: { id: string }) {
  const user = await getUser(id);
  return (
    <section>
      <UserCard user={user} />
      <UserBadge profile={user} />
    </section>
  );
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 2);

        let badge = reads
            .iter()
            .find(|r| r.read_file == "src/components/UserBadge.tsx")
            .expect("UserBadge prop read should be detected");
        assert_eq!(badge.field_path, "email");
        assert_eq!(badge.read_line, 2);
        assert_eq!(badge.origin_file, "src/services/user.ts");
        assert_eq!(badge.confidence, Confidence::Confirmed);

        let card = reads
            .iter()
            .find(|r| r.read_file == "src/components/UserCard.tsx")
            .expect("UserCard prop read should be detected");
        assert_eq!(card.field_path, "segundoApellido");
        assert_eq!(card.read_line, 2);
        assert_eq!(card.origin_file, "src/services/user.ts");
        assert_eq!(card.confidence, Confidence::Confirmed);
        assert!(
            card.trace
                .iter()
                .any(|t| t.contains("JSX prop `user` -> <UserCard>"))
        );
    }

    #[test]
    fn barrel_index_reexport_and_exported_service_object_propagate() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/services/userService.ts".to_string(),
                r#"export const portalServices = {
  async getUser(id: string) {
    const res = await fetch(`http://user-service/users/${id}`);
    return res.json();
  },
};
"#
                .to_string(),
            ),
            (
                "src/services/index.ts".to_string(),
                "export * from './userService';\n".to_string(),
            ),
            (
                "src/pages/Dashboard.tsx".to_string(),
                r#"import { portalServices } from '../services';

export async function loadDashboard(id: string) {
  const user = await portalServices.getUser(id);
  return user?.segundoApellido;
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 1);
        let read = &reads[0];
        assert_eq!(read.service, "user-service");
        assert_eq!(read.method, HttpMethod::Get);
        assert_eq!(read.normalized_path, "/users/:id");
        assert_eq!(read.field_path, "segundoApellido");
        assert_eq!(read.read_file, "src/pages/Dashboard.tsx");
        assert_eq!(read.read_line, 5);
        assert_eq!(read.origin_file, "src/services/userService.ts");
        assert_eq!(read.origin_line, 3);
        assert_eq!(read.confidence, Confidence::Confirmed);
    }

    #[test]
    fn vue_sfc_template_consumes_imported_service_and_composable() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/services/user.ts".to_string(),
                r#"export async function getUser(id: string) {
  const res = await fetch(`http://user-service/users/${id}`);
  return res.json();
}
"#
                .to_string(),
            ),
            (
                "src/composables/useUser.ts".to_string(),
                r#"import { ref } from 'vue';
import { getUser } from '../services/user';

export function useUser(id: string) {
  const user = ref(null);
  async function load() {
    user.value = await getUser(id);
  }
  load();
  return { user, load };
}
"#
                .to_string(),
            ),
            (
                "src/components/DirectUser.vue".to_string(),
                r#"<template>
  <p>{{ user.segundoApellido }}</p>
</template>

<script setup lang="ts">
import { ref } from 'vue';
import { getUser } from '../services/user';

const user = ref(null);
async function init(id: string) {
  user.value = await getUser(id);
}
</script>
"#
                .to_string(),
            ),
            (
                "src/components/ComposableUser.vue".to_string(),
                r#"<template>
  <p>{{ user.email }}</p>
</template>

<script setup lang="ts">
import { useUser } from '../composables/useUser';

const { user } = useUser('42');
</script>
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        let direct = reads
            .iter()
            .find(|r| r.read_file == "src/components/DirectUser.vue")
            .expect("Vue SFC direct ref template read should be detected");
        assert_eq!(direct.field_path, "segundoApellido");
        assert_eq!(direct.read_line, 2);
        assert_eq!(direct.origin_file, "src/services/user.ts");
        assert_eq!(direct.confidence, Confidence::Confirmed);

        let composable = reads
            .iter()
            .find(|r| r.read_file == "src/components/ComposableUser.vue")
            .expect("Vue SFC composable template read should be detected");
        assert_eq!(composable.field_path, "email");
        assert_eq!(composable.read_line, 2);
        assert_eq!(composable.origin_file, "src/services/user.ts");
        assert_eq!(composable.confidence, Confidence::Confirmed);
    }

    #[test]
    fn cyclic_imports_terminate_cleanly_without_infinite_recursion() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/a.ts".to_string(),
                r#"import { fromB } from './b';
export * from './b';
export async function fromA(id: string) {
  return fromB(id);
}
"#
                .to_string(),
            ),
            (
                "src/b.ts".to_string(),
                r#"import { fromA } from './a';
export * from './a';
export async function fromB(id: string) {
  return fromA(id);
}
"#
                .to_string(),
            ),
            (
                "src/consumer.ts".to_string(),
                r#"import { fromA } from './a';
export async function run(id: string) {
  const user = await fromA(id);
  return user.segundoApellido;
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);
        assert!(reads.is_empty());
    }

    #[test]
    fn cross_file_helper_custom_client_zod_parse_and_swr_work() {
        let services = user_services();
        let clients = vec![CustomClientConfig {
            callee: "getClient().fetch".into(),
            target_service: "user-service".into(),
            path_prefix: Some("/api".into()),
        }];
        let mut env = BTreeMap::new();
        env.insert("VITE_API".to_string(), "http://user-service".to_string());
        let ctx = ExtractContext {
            services: &services,
            clients: &clients,
            env: &env,
        };

        let files = vec![
            (
                "src/services/client.ts".to_string(),
                r#"export async function fetchUser(id: string) {
  const raw = await getClient().fetch(`/users/${id}`);
  return UserSchema.parse(raw);
}
"#
                .to_string(),
            ),
            (
                "src/hooks/useUserSwr.ts".to_string(),
                r#"import useSWR from 'swr';
import { fetchUser } from '../services/client';

export function useUserSwr(id: string) {
  return useSWR(['user', id], () => fetchUser(id));
}
"#
                .to_string(),
            ),
            (
                "src/utils/format.ts".to_string(),
                r#"export function formatSurname(u: any) {
  return u.segundoApellido.trim();
}
"#
                .to_string(),
            ),
            (
                "src/pages/Detail.tsx".to_string(),
                r#"import { useUserSwr } from '../hooks/useUserSwr';
import { formatSurname } from '../utils/format';

export function Detail({ id }: { id: string }) {
  const { data: user } = useUserSwr(id);
  return <p>{formatSurname(user)}</p>;
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 1);
        let read = &reads[0];
        assert_eq!(read.service, "user-service");
        assert_eq!(read.normalized_path, "/api/users/:id");
        assert_eq!(read.field_path, "segundoApellido");
        assert_eq!(read.read_file, "src/utils/format.ts");
        assert_eq!(read.read_line, 2);
        assert_eq!(read.origin_file, "src/services/client.ts");
        assert_eq!(read.confidence, Confidence::Likely);
    }

    #[test]
    fn same_file_fetch_passed_into_imported_jsx_component_emits_cross_file_read() {
        let services = user_services();
        let empty_env = BTreeMap::new();
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &empty_env,
        };

        let files = vec![
            (
                "src/components/UserCard.tsx".to_string(),
                r#"export function UserCard({ user }: { user: any }) {
  return <p>{user.segundoApellido}</p>;
}
"#
                .to_string(),
            ),
            (
                "src/pages/InlineFetch.tsx".to_string(),
                r#"import { UserCard } from '../components/UserCard';

export async function InlineFetch({ id }: { id: string }) {
  const user = await fetch(`http://user-service/users/${id}`).then((r) => r.json());
  return <UserCard user={user} />;
}
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 1);
        let read = &reads[0];
        assert_eq!(read.service, "user-service");
        assert_eq!(read.normalized_path, "/users/:id");
        assert_eq!(read.field_path, "segundoApellido");
        assert_eq!(read.read_file, "src/components/UserCard.tsx");
        assert_eq!(read.read_line, 2);
        assert_eq!(read.origin_file, "src/pages/InlineFetch.tsx");
        assert_eq!(read.origin_line, 4);
        assert_eq!(read.confidence, Confidence::Confirmed);
    }

    #[test]
    fn axios_create_env_default_export_service_and_nuxt_async_data_work() {
        let services = user_services();
        let mut env = BTreeMap::new();
        env.insert("VITE_API".to_string(), "http://user-service".to_string());
        let ctx = ExtractContext {
            services: &services,
            clients: &[],
            env: &env,
        };

        let files = vec![
            (
                "src/services/defaultUser.ts".to_string(),
                r#"import axios from 'axios';

const api = axios.create({ baseURL: import.meta.env.VITE_API });

export default {
  async getUser(id: string) {
    const resp = await api.get(`/users/${id}`);
    return resp.data;
  },
};
"#
                .to_string(),
            ),
            (
                "src/composables/useUserAsync.ts".to_string(),
                r#"import userApi from '../services/defaultUser';

export function useUserAsync(id: string) {
  return useAsyncData(`user-${id}`, () => userApi.getUser(id));
}
"#
                .to_string(),
            ),
            (
                "src/pages/UserPage.vue".to_string(),
                r#"<template>
  <h1>{{ data.segundoApellido }}</h1>
</template>

<script setup lang="ts">
import { useUserAsync } from '../composables/useUserAsync';

const { data } = await useUserAsync('99');
</script>
"#
                .to_string(),
            ),
        ];

        let resolver = make_test_resolver(&files);
        let reads = analyze_cross_file_dataflow(&files, ctx, &resolver);

        assert_eq!(reads.len(), 1);
        let read = &reads[0];
        assert_eq!(read.service, "user-service");
        assert_eq!(read.method, HttpMethod::Get);
        assert_eq!(read.normalized_path, "/users/:id");
        assert_eq!(read.field_path, "segundoApellido");
        assert_eq!(read.read_file, "src/pages/UserPage.vue");
        assert_eq!(read.read_line, 2);
        assert_eq!(read.origin_file, "src/services/defaultUser.ts");
        assert_eq!(read.origin_line, 7);
        assert_eq!(read.confidence, Confidence::Likely);
    }
}
