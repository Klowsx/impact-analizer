use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use laine_core::config::{CustomClientConfig, ServiceConfig};
use laine_core::model::HttpMethod;
use laine_core::request::{Argument, ArgumentType, HttpRequestEvidence, RequestSection};
use laine_core::url::{resolve_custom_client_url, resolve_http_service_url, resolve_url_with_base};
use serde::{Deserialize, Serialize};

use crate::routes::{
    CallArg, detect_string_start, find_matching_delim, is_ident_continue, is_ident_start,
    parse_call_args, parse_python_string_literal, read_ident, scan_single_line_string, skip_ws,
    split_top_level_comma, strip_python_comments_and_docstrings, try_skip_string_literal,
    unescape_python_string,
};

const UNMAPPED_ENV: &str = "unmapped environment variable (configure under `[env]` in laine.toml)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PyHttpClientKind {
    Requests,
    Httpx,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PyHttpCall {
    pub id: usize,
    pub client: PyHttpClientKind,
    pub service: String,
    pub method: HttpMethod,
    pub normalized_path: String,
    #[serde(default)]
    pub request: HttpRequestEvidence,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PyFieldRead {
    pub call_id: usize,
    pub field_path: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PyUnresolvedCall {
    pub client: PyHttpClientKind,
    pub line: usize,
    pub snippet: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PyFileExtraction {
    pub file_path: String,
    pub calls: Vec<PyHttpCall>,
    pub field_reads: Vec<PyFieldRead>,
    pub unresolved_calls: Vec<PyUnresolvedCall>,
}

impl PyFileExtraction {
    /// Removes original source lines before persisting extracted consumer facts.
    pub fn clear_snippets(&mut self) {
        for call in &mut self.calls {
            call.snippet.clear();
        }
        for read in &mut self.field_reads {
            read.snippet.clear();
        }
        for unresolved in &mut self.unresolved_calls {
            unresolved.snippet.clear();
        }
    }
}

/// Workspace context passed to the Python HTTP consumer extractor.
#[derive(Debug, Clone, Copy)]
pub struct PyExtractContext<'a> {
    pub services: &'a [ServiceConfig],
    pub clients: &'a [CustomClientConfig],
    pub env: &'a BTreeMap<String, String>,
}

/// Extracts Python HTTP consumer calls (`requests`, `httpx`, `httpx.Client` / `httpx.AsyncClient`,
/// and configured `[[clients]]` wrappers), response JSON field reads (`customer["fullName"]`,
/// `customer.get("email")`, chained `.json()["field"]`, and local aliases), and unresolved
/// dynamic URLs (`UNKNOWN`) with indentation-based function scoping.
pub fn extract_python_file(
    file_path: &str,
    source: &str,
    ctx: PyExtractContext<'_>,
) -> PyFileExtraction {
    let cleaned = strip_python_comments_and_docstrings(source);
    let raw_lines: Vec<&str> = source.lines().collect();
    let statements = split_logical_statements(&cleaned);

    let mut extractor = PyFileExtractor::new(file_path, &raw_lines, ctx);
    for stmt in &statements {
        extractor.process_statement(stmt);
    }
    extractor.finish()
}

#[derive(Debug, Clone)]
struct LogicalStmt {
    indent: usize,
    start_line: usize,
    global_offset: usize,
    text: String,
}

#[derive(Debug, Clone)]
enum UrlEval {
    Resolved(String),
    Dynamic(String),
}

#[derive(Debug, Clone)]
enum VarBinding {
    HttpImport(String),
    StringLiteral(String),
    DynamicString(String),
    HttpClientInstance {
        client: PyHttpClientKind,
        base_url: Option<UrlEval>,
    },
    HttpResponse(usize),
    JsonPayload {
        call_id: usize,
        prefix: String,
    },
    Unknown,
}

#[derive(Debug, Clone)]
struct Scope {
    min_indent: usize,
    vars: HashMap<String, VarBinding>,
}

#[derive(Debug, Clone)]
enum CallTarget {
    Requests {
        method: HttpMethod,
        url_arg_index: usize,
    },
    Httpx {
        method: HttpMethod,
        url_arg_index: usize,
    },
    ClientInstance {
        client: PyHttpClientKind,
        base_url: Option<UrlEval>,
        method: HttpMethod,
        url_arg_index: usize,
    },
    Custom {
        target_service: String,
        path_prefix: Option<String>,
        method: HttpMethod,
        url_arg_index: usize,
    },
}

#[derive(Debug, Clone)]
enum PostfixOp {
    Attr {
        name: String,
    },
    Call {
        raw_args: String,
        offset: usize,
        args_offset: usize,
    },
    Subscript {
        raw_index: String,
        offset: usize,
        index_offset: usize,
    },
}

struct PyFileExtractor<'a> {
    file_path: String,
    raw_lines: &'a [&'a str],
    ctx: PyExtractContext<'a>,
    scopes: Vec<Scope>,
    calls: Vec<PyHttpCall>,
    field_reads: Vec<PyFieldRead>,
    unresolved_calls: Vec<PyUnresolvedCall>,
    seen_calls: HashSet<usize>,
    seen_field_reads: HashSet<(usize, String, usize)>,
}

impl<'a> PyFileExtractor<'a> {
    fn new(file_path: &str, raw_lines: &'a [&'a str], ctx: PyExtractContext<'a>) -> Self {
        Self {
            file_path: file_path.to_string(),
            raw_lines,
            ctx,
            scopes: vec![Scope {
                min_indent: 0,
                vars: HashMap::new(),
            }],
            calls: Vec::new(),
            field_reads: Vec::new(),
            unresolved_calls: Vec::new(),
            seen_calls: HashSet::new(),
            seen_field_reads: HashSet::new(),
        }
    }

    fn finish(self) -> PyFileExtraction {
        PyFileExtraction {
            file_path: self.file_path,
            calls: self.calls,
            field_reads: self.field_reads,
            unresolved_calls: self.unresolved_calls,
        }
    }

    fn pop_scopes_for_indent(&mut self, indent: usize) {
        while self.scopes.len() > 1
            && self
                .scopes
                .last()
                .is_some_and(|scope| indent <= scope.min_indent)
        {
            self.scopes.pop();
        }
    }

    fn push_scope(&mut self, min_indent: usize) {
        self.scopes.push(Scope {
            min_indent,
            vars: HashMap::new(),
        });
    }

    fn bind_var(&mut self, name: &str, binding: VarBinding) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.vars.insert(name.to_string(), binding);
        }
    }

    fn lookup_var(&self, name: &str) -> Option<&VarBinding> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.vars.get(name) {
                return Some(binding);
            }
        }
        None
    }

    fn line_and_snippet(&self, stmt: &LogicalStmt, rel_offset: usize) -> (usize, String) {
        let clamped = rel_offset.min(stmt.text.len());
        let extra_lines = stmt.text[..clamped].bytes().filter(|&b| b == b'\n').count();
        let line = stmt.start_line + extra_lines;
        let snippet = self
            .raw_lines
            .get(line.saturating_sub(1))
            .map(|l| l.trim().to_string())
            .unwrap_or_default();
        (line, snippet)
    }

    fn record_field_read(
        &mut self,
        stmt: &LogicalStmt,
        call_id: usize,
        field_path: String,
        rel_offset: usize,
    ) {
        if field_path.is_empty() {
            return;
        }
        let (line, snippet) = self.line_and_snippet(stmt, rel_offset);
        if self
            .seen_field_reads
            .insert((call_id, field_path.clone(), line))
        {
            self.field_reads.push(PyFieldRead {
                call_id,
                field_path,
                line,
                snippet,
            });
        }
    }

    // Procesa sentencias con el ámbito activo para que los aliases no se filtren a otros ámbitos.
    fn process_statement(&mut self, stmt: &LogicalStmt) {
        let trimmed = stmt.text.trim();
        if trimmed.is_empty() {
            return;
        }

        // Ignore decorators in consumer extraction (scope pop still applies at decorator indent).
        self.pop_scopes_for_indent(stmt.indent);
        if trimmed.starts_with('@')
            || trimmed == "pass"
            || trimmed == "break"
            || trimmed == "continue"
        {
            return;
        }

        // 1. `def` / `async def` / `class` header -> push new function/class scope.
        if let Some(header) = parse_def_or_class_header(trimmed) {
            if let Some(name) = header.name {
                self.bind_var(name, VarBinding::Unknown);
            }
            self.push_scope(stmt.indent);
            for param in header.params {
                self.bind_var(&param, VarBinding::Unknown);
            }
            if let Some((body_text, body_rel)) = header.inline_body {
                let body_offset = stmt.text.len() - stmt.text.trim_start().len() + body_rel;
                self.process_simple_stmt(stmt, body_text, body_offset);
            }
            return;
        }

        // 2. `with` / `async with` statement
        let leading_ws = stmt.text.len() - stmt.text.trim_start().len();
        if let Some(rest_rel) = strip_keyword_prefix(trimmed, "async with")
            .or_else(|| strip_keyword_prefix(trimmed, "with"))
        {
            let rest = &trimmed[rest_rel..];
            let rest_offset = leading_ws + rest_rel;
            self.process_with_stmt(stmt, rest, rest_offset);
            return;
        }

        self.process_simple_stmt(stmt, trimmed, leading_ws);
    }

    fn process_with_stmt(&mut self, stmt: &LogicalStmt, rest: &str, base_offset: usize) {
        let (header, inline_body) = split_header_colon(rest);
        for (item, rel) in split_top_level_comma(header) {
            let item_trimmed = item.trim();
            if item_trimmed.is_empty() {
                continue;
            }
            let item_ws = item.len() - item.trim_start().len();
            let item_offset = base_offset + rel + item_ws;

            if let Some((expr_str, var_name)) = split_with_as(item_trimmed) {
                let binding = self.eval_expr_binding(stmt, expr_str, item_offset);
                if is_simple_ident(var_name) {
                    self.bind_var(var_name, binding.unwrap_or(VarBinding::Unknown));
                }
            } else {
                self.eval_expr_binding(stmt, item_trimmed, item_offset);
            }
        }

        if let Some((body_text, body_rel)) = inline_body {
            self.process_simple_stmt(stmt, body_text, base_offset + body_rel);
        }
    }

    // Guarda imports y analiza el resto de las sentencias del ámbito actual.
    fn process_simple_stmt(&mut self, stmt: &LogicalStmt, text: &str, base_offset: usize) {
        let trimmed = text.trim();
        if trimmed.is_empty() || self.record_import(trimmed) {
            return;
        }
        let ws = text.len() - text.trim_start().len();
        let offset = base_offset + ws;

        // Compound headers (`if`, `elif`, `while`, `for`, `else`, `try`, `except`, `finally`)
        if let Some(rest_rel) = strip_keyword_prefix(trimmed, "for")
            .or_else(|| strip_keyword_prefix(trimmed, "async for"))
        {
            let rest = &trimmed[rest_rel..];
            let (header, inline_body) = split_header_colon(rest);
            if let Some(in_pos) = find_top_level_keyword(header, "in") {
                let iter_start = in_pos + 2;
                let iter_expr = &header[iter_start..];
                let iter_ws = iter_expr.len() - iter_expr.trim_start().len();
                self.eval_expr_binding(
                    stmt,
                    iter_expr.trim(),
                    offset + rest_rel + iter_start + iter_ws,
                );
            }
            if let Some((body, body_rel)) = inline_body {
                self.process_simple_stmt(stmt, body, offset + rest_rel + body_rel);
            }
            return;
        }

        for kw in ["if", "elif", "while"] {
            if let Some(rest_rel) = strip_keyword_prefix(trimmed, kw) {
                let rest = &trimmed[rest_rel..];
                let (cond, inline_body) = split_header_colon(rest);
                self.eval_expr_binding(stmt, cond.trim(), offset + rest_rel);
                if let Some((body, body_rel)) = inline_body {
                    self.process_simple_stmt(stmt, body, offset + rest_rel + body_rel);
                }
                return;
            }
        }

        for kw in ["else", "try", "finally", "except"] {
            if trimmed == kw
                || trimmed.starts_with(&format!("{kw}:"))
                || strip_keyword_prefix(trimmed, kw).is_some()
            {
                let (_, inline_body) = split_header_colon(trimmed);
                if let Some((body, body_rel)) = inline_body {
                    self.process_simple_stmt(stmt, body, offset + body_rel);
                }
                return;
            }
        }

        for kw in ["return", "yield from", "yield", "assert", "raise"] {
            if let Some(rest_rel) = strip_keyword_prefix(trimmed, kw) {
                let expr = &trimmed[rest_rel..];
                let expr_ws = expr.len() - expr.trim_start().len();
                self.eval_expr_binding(stmt, expr.trim(), offset + rest_rel + expr_ws);
                return;
            }
        }

        // Assignment statement: `<lhs> = <rhs>` or `<lhs>: <type> = <rhs>`
        if let Some((lhs, rhs, rhs_rel)) = split_assignment(trimmed) {
            let rhs_trimmed = rhs.trim();
            let rhs_ws = rhs.len() - rhs.trim_start().len();
            let rhs_offset = offset + rhs_rel + rhs_ws;
            let binding = self.eval_expr_binding(stmt, rhs_trimmed, rhs_offset);

            let lhs_target = strip_type_annotation(lhs).trim();
            if is_simple_ident(lhs_target) {
                self.bind_var(lhs_target, binding.unwrap_or(VarBinding::Unknown));
            } else {
                // Scan complex LHS (e.g. `obj[k] = ...`) for any nested expressions.
                self.eval_expr_binding(stmt, lhs_target, offset);
            }
            return;
        }

        // Plain expression statement
        self.eval_expr_binding(stmt, trimmed, offset);
    }

    // Registra módulos y símbolos HTTP importados y permite que otros imports oculten el nombre.
    fn record_import(&mut self, text: &str) -> bool {
        let (module, items) = if let Some(start) = strip_keyword_prefix(text, "import") {
            (None, &text[start..])
        } else if let Some(start) = strip_keyword_prefix(text, "from") {
            let rest = text[start..].trim_start();
            let Some((module, clause)) = rest.split_once(char::is_whitespace) else {
                return true;
            };
            let clause = clause.trim_start();
            let Some(start) = strip_keyword_prefix(clause, "import") else {
                return true;
            };
            (Some(module), &clause[start..])
        } else {
            return false;
        };

        for item in items
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
        {
            let mut words = item.split_whitespace();
            let Some(name) = words.next() else {
                continue;
            };
            let alias = if words.next() == Some("as") {
                words.next()
            } else {
                None
            };
            let root = name.split('.').next().unwrap_or(name);
            let binding = match module {
                Some(module @ ("requests" | "httpx")) => Some(format!("{module}.{name}")),
                None if matches!(root, "requests" | "httpx")
                    && (alias.is_none() || name == root) =>
                {
                    Some(root.to_owned())
                }
                _ => None,
            };
            let local_name = alias.unwrap_or(if module.is_some() { name } else { root });
            if is_simple_ident(local_name) {
                self.bind_var(
                    local_name,
                    binding.map_or(VarBinding::Unknown, VarBinding::HttpImport),
                );
            }
        }
        true
    }

    /// Evaluates a Python expression, recording any HTTP calls, unresolved calls, and field reads,
    /// and returning the resulting data-flow `VarBinding` when trackable.
    fn eval_expr_binding(
        &mut self,
        stmt: &LogicalStmt,
        raw_expr: &str,
        base_offset: usize,
    ) -> Option<VarBinding> {
        let (expr, offset) = unwrap_await_and_parens(raw_expr, base_offset);
        if expr.is_empty() {
            return None;
        }

        // 1. Static string literal
        if let Some(s) = parse_python_string_literal(expr) {
            return Some(VarBinding::StringLiteral(s));
        }

        // 2. Single f-string literal: scan interpolations for calls/field reads AND evaluate string.
        if is_single_fstring(expr) {
            self.scan_fstring_interpolations(stmt, expr, offset);
            return match self.eval_fstring_url(expr) {
                UrlEval::Resolved(s) => Some(VarBinding::StringLiteral(s)),
                UrlEval::Dynamic(reason) => Some(VarBinding::DynamicString(reason)),
            };
        }

        // 3. Container literals: `[...]`, `{...}`, `(...)` (tuple)
        if let Some(first) = expr.as_bytes().first().copied()
            && matches!(first, b'[' | b'{' | b'(')
        {
            let close = match first {
                b'[' => b']',
                b'{' => b'}',
                _ => b')',
            };
            if find_matching_delim(expr.as_bytes(), 0, first, close) == Some(expr.len() - 1) {
                self.scan_container_inner(
                    stmt,
                    &expr[1..expr.len() - 1],
                    offset + 1,
                    first == b'{',
                );
                return None;
            }
        }

        // 4. Compound expression (binary ops, boolean ops, ternary, etc.)
        if let Some(parts) = split_compound_expression(expr) {
            // Check if this is a string concatenation expression (`BASE_URL + "/customers"`).
            let concat_binding = if is_plus_only_chain(expr) {
                match self.eval_url_expr(expr) {
                    UrlEval::Resolved(s) => Some(VarBinding::StringLiteral(s)),
                    UrlEval::Dynamic(reason) => Some(VarBinding::DynamicString(reason)),
                }
            } else {
                None
            };

            for (part, rel) in parts {
                self.eval_expr_binding(stmt, part, offset + rel);
            }
            return concat_binding;
        }

        // 5. Postfix chain (`primary(.attr | (args) | [idx])*`)
        self.eval_postfix_chain(stmt, expr, offset)
    }

    fn scan_container_inner(
        &mut self,
        stmt: &LogicalStmt,
        inner: &str,
        base_offset: usize,
        is_brace: bool,
    ) {
        for (item, rel) in split_top_level_comma(inner) {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            let ws = item.len() - item.trim_start().len();
            let item_offset = base_offset + rel + ws;

            if is_brace && let Some(colon_pos) = find_top_level_byte(trimmed, b':') {
                let key = &trimmed[..colon_pos];
                let val = &trimmed[colon_pos + 1..];
                let val_ws = val.len() - val.trim_start().len();
                self.eval_expr_binding(stmt, key.trim(), item_offset);
                self.eval_expr_binding(stmt, val.trim(), item_offset + colon_pos + 1 + val_ws);
            } else {
                self.eval_expr_binding(stmt, trimmed, item_offset);
            }
        }
    }

    fn scan_fstring_interpolations(&mut self, stmt: &LogicalStmt, fstr: &str, base_offset: usize) {
        for (expr_str, rel) in extract_fstring_expressions(fstr) {
            self.eval_expr_binding(stmt, expr_str, base_offset + rel);
        }
    }

    // Sigue llamadas y accesos encadenados sin incluir claves de texto en los diagnósticos.
    fn eval_postfix_chain(
        &mut self,
        stmt: &LogicalStmt,
        expr: &str,
        base_offset: usize,
    ) -> Option<VarBinding> {
        let (primary_str, primary_rel, ops) = parse_postfix_chain(expr)?;
        let primary_offset = base_offset + primary_rel;

        // Evaluate the primary expression.
        let mut current_binding: Option<VarBinding> = None;
        let mut callee_text: Option<String> = None;

        if primary_str.starts_with('(') && primary_str.ends_with(')') {
            current_binding = self.eval_expr_binding(
                stmt,
                &primary_str[1..primary_str.len() - 1],
                primary_offset + 1,
            );
        } else if is_simple_ident(primary_str) {
            current_binding = self.lookup_var(primary_str).cloned();
            callee_text = Some(primary_str.to_string());
        } else if let Some(s) = parse_python_string_literal(primary_str) {
            current_binding = Some(VarBinding::StringLiteral(s));
        } else if is_single_fstring(primary_str) {
            self.scan_fstring_interpolations(stmt, primary_str, primary_offset);
            current_binding = match self.eval_fstring_url(primary_str) {
                UrlEval::Resolved(s) => Some(VarBinding::StringLiteral(s)),
                UrlEval::Dynamic(reason) => Some(VarBinding::DynamicString(reason)),
            };
        } else if (primary_str.starts_with('[') && primary_str.ends_with(']'))
            || (primary_str.starts_with('{') && primary_str.ends_with('}'))
        {
            self.scan_container_inner(
                stmt,
                &primary_str[1..primary_str.len() - 1],
                primary_offset + 1,
                primary_str.starts_with('{'),
            );
        }

        let mut pending_attr: Option<String> = None;
        let mut pending_field_read: Option<(usize, String, usize)> = None;

        for op in ops {
            match op {
                PostfixOp::Attr { name } => {
                    callee_text = callee_text.map(|c| format!("{c}.{name}"));
                    pending_attr = Some(name);
                }
                PostfixOp::Call {
                    raw_args,
                    offset,
                    args_offset,
                } => {
                    let call_rel_offset = base_offset + offset;
                    let args_base_offset = base_offset + args_offset;
                    let args = parse_call_args(&raw_args, args_base_offset);
                    let method_name = pending_attr.take();

                    // 1. Check `os.getenv(...)` / `os.environ.get(...)`
                    if matches!(callee_text.as_deref(), Some("os.getenv" | "os.environ.get")) {
                        if let Some((call_id, path, f_off)) = pending_field_read.take() {
                            self.record_field_read(stmt, call_id, path, f_off);
                        }
                        current_binding = match self.eval_env_call_args(&args) {
                            UrlEval::Resolved(val) => Some(VarBinding::StringLiteral(val)),
                            UrlEval::Dynamic(reason) => Some(VarBinding::DynamicString(reason)),
                        };
                        callee_text = callee_text.map(|c| format!("{c}()"));
                        continue;
                    }

                    // 2. Check `httpx.Client(...)`, `httpx.AsyncClient(...)`, `requests.Session()`
                    if let Some(instance) =
                        self.try_parse_client_ctor(callee_text.as_deref(), &args)
                    {
                        if let Some((call_id, path, f_off)) = pending_field_read.take() {
                            self.record_field_read(stmt, call_id, path, f_off);
                        }
                        // Scan non-base_url args for any nested expressions.
                        for arg in &args {
                            if arg.name.as_deref() != Some("base_url") {
                                self.eval_expr_binding(stmt, &arg.value, arg.offset);
                            }
                        }
                        current_binding = Some(instance);
                        callee_text = callee_text.map(|c| format!("{c}()"));
                        continue;
                    }

                    // 3. Check HTTP call target (custom clients, requests, httpx, client instance)
                    if let Some(target) = self.classify_http_call(
                        callee_text.as_deref(),
                        current_binding.as_ref(),
                        method_name.as_deref(),
                        &args,
                    ) {
                        if let Some((call_id, path, f_off)) = pending_field_read.take() {
                            self.record_field_read(stmt, call_id, path, f_off);
                        }
                        current_binding =
                            self.process_http_call(stmt, target, &args, call_rel_offset);
                        callee_text = callee_text.map(|c| format!("{c}()"));
                        continue;
                    }

                    // 4. `res.json()` on an HttpResponse or Custom client JsonPayload
                    if method_name.as_deref() == Some("json") {
                        match &current_binding {
                            Some(VarBinding::HttpResponse(call_id)) => {
                                current_binding = Some(VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: String::new(),
                                });
                                callee_text = callee_text.map(|c| format!("{c}()"));
                                continue;
                            }
                            Some(VarBinding::JsonPayload { call_id, prefix })
                                if prefix.is_empty() =>
                            {
                                current_binding = Some(VarBinding::JsonPayload {
                                    call_id: *call_id,
                                    prefix: String::new(),
                                });
                                callee_text = callee_text.map(|c| format!("{c}()"));
                                continue;
                            }
                            _ => {}
                        }
                    }

                    // 5. `customer.get("fullName")` / `customer.pop("fullName")` on JsonPayload
                    if matches!(method_name.as_deref(), Some("get" | "pop"))
                        && let Some(VarBinding::JsonPayload { call_id, prefix }) =
                            current_binding.clone()
                        && let Some(key_arg) = args.iter().find(|a| a.name.is_none())
                        && let Some(key) = parse_python_string_literal(&key_arg.value)
                    {
                        let full_path = join_field_path(&prefix, &key);
                        pending_field_read = Some((call_id, full_path.clone(), call_rel_offset));
                        current_binding = Some(VarBinding::JsonPayload {
                            call_id,
                            prefix: full_path,
                        });
                        // Scan any remaining args (e.g. default fallback value).
                        for arg in args.iter().skip(1) {
                            self.eval_expr_binding(stmt, &arg.value, arg.offset);
                        }
                        callee_text = callee_text.map(|c| format!("{c}()"));
                        continue;
                    }

                    // 6. Any other function/method call: flush pending field read & scan args.
                    if let Some((call_id, path, f_off)) = pending_field_read.take() {
                        self.record_field_read(stmt, call_id, path, f_off);
                    }
                    for arg in &args {
                        self.eval_expr_binding(stmt, &arg.value, arg.offset);
                    }
                    current_binding = None;
                    callee_text = callee_text.map(|c| format!("{c}()"));
                }
                PostfixOp::Subscript {
                    raw_index,
                    offset,
                    index_offset,
                } => {
                    let sub_rel_offset = base_offset + offset;
                    let idx_base_offset = base_offset + index_offset;
                    pending_attr = None;

                    // 1. `os.environ["KEY"]`
                    if callee_text.as_deref() == Some("os.environ") {
                        if let Some(key) = parse_python_string_literal(&raw_index) {
                            current_binding = match self.ctx.env.get(&key) {
                                Some(val) => Some(VarBinding::StringLiteral(val.clone())),
                                None => Some(VarBinding::DynamicString(UNMAPPED_ENV.into())),
                            };
                        } else {
                            current_binding = Some(VarBinding::DynamicString(
                                "dynamic os.environ key cannot be resolved statically".to_string(),
                            ));
                        }
                        callee_text = None;
                        continue;
                    }

                    // 2. `customer["fullName"]` or `customer["items"][0]["name"]`
                    if let Some(VarBinding::JsonPayload { call_id, prefix }) =
                        current_binding.clone()
                    {
                        if let Some(key) = parse_python_string_literal(&raw_index) {
                            let full_path = join_field_path(&prefix, &key);
                            pending_field_read = Some((call_id, full_path.clone(), sub_rel_offset));
                            current_binding = Some(VarBinding::JsonPayload {
                                call_id,
                                prefix: full_path,
                            });
                            callee_text = None;
                            continue;
                        }
                        if is_integer_literal(raw_index.trim()) {
                            // Keep the JsonPayload binding alive across list indexing (e.g. `data["items"][0]["name"]`).
                            callee_text = None;
                            continue;
                        }
                    }

                    // 3. Any other subscript: flush pending read & scan index expression.
                    if let Some((call_id, path, f_off)) = pending_field_read.take() {
                        self.record_field_read(stmt, call_id, path, f_off);
                    }
                    self.eval_expr_binding(stmt, raw_index.trim(), idx_base_offset);
                    current_binding = None;
                    callee_text = None;
                }
            }
        }

        if let Some((call_id, path, f_off)) = pending_field_read {
            self.record_field_read(stmt, call_id, path, f_off);
        }

        current_binding
    }

    // Construye el cliente a partir del nombre importado del constructor.
    fn try_parse_client_ctor(&self, callee: Option<&str>, args: &[CallArg]) -> Option<VarBinding> {
        match self.resolve_http_callee(callee?)?.as_ref() {
            "httpx.Client" | "httpx.AsyncClient" => {
                let base_url = args
                    .iter()
                    .find(|a| a.name.as_deref() == Some("base_url"))
                    .map(|a| self.eval_url_expr(&a.value));
                Some(VarBinding::HttpClientInstance {
                    client: PyHttpClientKind::Httpx,
                    base_url,
                })
            }
            "requests.Session" => Some(VarBinding::HttpClientInstance {
                client: PyHttpClientKind::Requests,
                base_url: None,
            }),
            _ => None,
        }
    }

    // Expande el alias de un import solo si su nombre sigue vinculado en el ámbito actual.
    fn resolve_http_callee<'b>(&'b self, callee: &'b str) -> Option<Cow<'b, str>> {
        let (root, tail) = callee
            .split_once('.')
            .map_or((callee, None), |(root, tail)| (root, Some(tail)));
        match self.lookup_var(root) {
            Some(VarBinding::HttpImport(symbol)) => Some(match tail {
                Some(tail) => Cow::Owned(format!("{symbol}.{tail}")),
                None => Cow::Borrowed(symbol),
            }),
            Some(_) => None,
            None => Some(Cow::Borrowed(callee)),
        }
    }

    // Clasifica llamadas al módulo, funciones importadas e instancias de cliente.
    fn classify_http_call(
        &self,
        callee: Option<&str>,
        receiver: Option<&VarBinding>,
        method_name: Option<&str>,
        args: &[CallArg],
    ) -> Option<CallTarget> {
        // 1. Configured custom clients from `[[clients]]`
        if let Some(callee_str) = callee {
            for client in self.ctx.clients {
                if callee_str == client.callee {
                    let method = extract_method_kwarg(args).unwrap_or(HttpMethod::Get);
                    return Some(CallTarget::Custom {
                        target_service: client.target_service.clone(),
                        path_prefix: client.path_prefix.clone(),
                        method,
                        url_arg_index: 0,
                    });
                }

                if let Some(rest) = callee_str.strip_prefix(&client.callee)
                    && let Some(m_name) = rest.strip_prefix('.')
                {
                    if m_name == "fetch" || m_name == "request" {
                        let (method, url_idx) = extract_request_method_and_url_idx(args, m_name);
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method,
                            url_arg_index: url_idx,
                        });
                    }
                    if let Some(method) = HttpMethod::parse(m_name) {
                        return Some(CallTarget::Custom {
                            target_service: client.target_service.clone(),
                            path_prefix: client.path_prefix.clone(),
                            method,
                            url_arg_index: 0,
                        });
                    }
                }
            }
        }

        let canonical_callee = callee.and_then(|callee| self.resolve_http_callee(callee));
        let callee = canonical_callee.as_deref();

        // 2. Built-in `requests.<method>` / `requests.request`
        if let Some(callee_str) = callee
            && let Some(m_name) = callee_str.strip_prefix("requests.")
        {
            if m_name == "request" {
                let (method, url_idx) = extract_request_method_and_url_idx(args, m_name);
                return Some(CallTarget::Requests {
                    method,
                    url_arg_index: url_idx,
                });
            }
            if let Some(method) = HttpMethod::parse(m_name) {
                return Some(CallTarget::Requests {
                    method,
                    url_arg_index: 0,
                });
            }
        }

        // 3. Built-in `httpx.<method>` / `httpx.request`
        if let Some(callee_str) = callee
            && let Some(m_name) = callee_str.strip_prefix("httpx.")
        {
            if m_name == "request" {
                let (method, url_idx) = extract_request_method_and_url_idx(args, m_name);
                return Some(CallTarget::Httpx {
                    method,
                    url_arg_index: url_idx,
                });
            }
            if let Some(method) = HttpMethod::parse(m_name) {
                return Some(CallTarget::Httpx {
                    method,
                    url_arg_index: 0,
                });
            }
        }

        // 4. Local `httpx.Client` / `httpx.AsyncClient` / `requests.Session` instance
        if let (Some(VarBinding::HttpClientInstance { client, base_url }), Some(m_name)) =
            (receiver, method_name)
        {
            if m_name == "request" {
                let (method, url_idx) = extract_request_method_and_url_idx(args, m_name);
                return Some(CallTarget::ClientInstance {
                    client: *client,
                    base_url: base_url.clone(),
                    method,
                    url_arg_index: url_idx,
                });
            }
            if let Some(method) = HttpMethod::parse(m_name) {
                return Some(CallTarget::ClientInstance {
                    client: *client,
                    base_url: base_url.clone(),
                    method,
                    url_arg_index: 0,
                });
            }
        }

        None
    }

    // Extract requests and httpx keyword arguments without retaining their values.
    #[allow(clippy::too_many_arguments)]
    fn request_evidence(
        &self,
        stmt: &LogicalStmt,
        args: &[CallArg],
        client: PyHttpClientKind,
        url: &str,
        line: usize,
        url_arg_index: usize,
        is_instance: bool,
    ) -> HttpRequestEvidence {
        let arg = |name| args.iter().find(|arg| arg.name.as_deref() == Some(name));
        let spread = args
            .iter()
            .any(|arg| arg.name.is_none() && arg.value.starts_with('*'));
        let section = |name, wire, lowercase| {
            if let Some(arg) = arg(name) {
                if arg.value == "None" {
                    return RequestSection::absent();
                }
                let mut section = RequestSection::absent();
                self.request_value(
                    stmt,
                    &arg.value,
                    arg.offset,
                    "",
                    wire,
                    lowercase,
                    &mut section,
                );
                if wire && !arg.value.starts_with('{') {
                    section.complete = false;
                }
                section
            } else if spread {
                RequestSection::default()
            } else {
                RequestSection::absent()
            }
        };
        let mut request = if client == PyHttpClientKind::Custom {
            return HttpRequestEvidence::default();
        } else {
            let other_body = ["data", "content", "files"]
                .iter()
                .any(|name| arg(name).is_some());
            let uncommon_positional =
                args.iter().filter(|arg| arg.name.is_none()).count() > url_arg_index + 1;
            let mut headers = section("headers", true, true);
            headers.complete = false;
            HttpRequestEvidence {
                body: if other_body || uncommon_positional || spread {
                    RequestSection::default()
                } else {
                    section("json", false, false)
                },
                query: if uncommon_positional && arg("params").is_none() {
                    RequestSection::default()
                } else {
                    section("params", true, false)
                },
                headers,
            }
        };
        // Client defaults or HTTPX params can replace query values embedded in the URL.
        if is_instance
            || (client == PyHttpClientKind::Httpx
                && arg("params").is_some_and(|param| param.value != "None")
                && url.split('#').next().is_some_and(|base| base.contains('?')))
        {
            request.query.complete = false;
        }
        request.query.add_url_query(url, line);
        request
    }

    // Walk dictionary literals and keep each observed type at its source line.
    #[allow(clippy::too_many_arguments)]
    fn request_value(
        &self,
        stmt: &LogicalStmt,
        raw: &str,
        offset: usize,
        path: &str,
        wire: bool,
        lowercase: bool,
        section: &mut RequestSection,
    ) {
        let trimmed = raw.trim_start();
        let offset = offset + raw.len() - trimmed.len();
        let value = trimmed.trim_end();
        let line = self.line_and_snippet(stmt, offset).0;
        let argument = python_argument(value, line, wire);
        if path.is_empty() {
            section.complete &= !argument.types.is_empty() || (wire && value.starts_with('{'));
            section.root = Some(argument);
        } else {
            let children = format!("{path}.");
            section
                .fields
                .retain(|name, _| !name.starts_with(&children));
            section.fields.insert(path.to_owned(), argument);
        }
        if find_matching_delim(value.as_bytes(), 0, b'{', b'}') != value.len().checked_sub(1)
            || !value.starts_with('{')
        {
            return;
        }
        if wire && !path.is_empty() {
            return;
        }
        for (entry, start) in split_top_level_comma(&value[1..value.len() - 1]) {
            let entry_trimmed = entry.trim_start();
            if entry_trimmed.is_empty() {
                continue;
            }
            let start = start + entry.len() - entry_trimmed.len();
            let Some((mut key, child, child_offset)) = python_dict_entry(entry_trimmed)
                .filter(|(name, _, _)| wire || !name.contains(['.', '[', ']']))
            else {
                section.complete = false;
                if path.is_empty() {
                    section.fields.clear();
                } else {
                    let children = format!("{path}.");
                    section
                        .fields
                        .retain(|name, _| !name.starts_with(&children));
                }
                continue;
            };
            if lowercase {
                key.make_ascii_lowercase();
            }
            let child_path = join_field_path(path, &key);
            self.request_value(
                stmt,
                child,
                offset + 1 + start + child_offset,
                &child_path,
                wire,
                lowercase,
                section,
            );
        }
    }

    // Record HTTP calls and keep diagnostics for unresolved destinations.
    fn process_http_call(
        &mut self,
        stmt: &LogicalStmt,
        target: CallTarget,
        args: &[CallArg],
        call_rel_offset: usize,
    ) -> Option<VarBinding> {
        let global_call_key = stmt.global_offset + call_rel_offset;
        if !self.seen_calls.insert(global_call_key) {
            return None;
        }

        let (client_kind, method, url_arg_index) = match &target {
            CallTarget::Requests {
                method,
                url_arg_index,
            } => (PyHttpClientKind::Requests, *method, *url_arg_index),
            CallTarget::Httpx {
                method,
                url_arg_index,
            } => (PyHttpClientKind::Httpx, *method, *url_arg_index),
            CallTarget::ClientInstance {
                client,
                method,
                url_arg_index,
                ..
            } => (*client, *method, *url_arg_index),
            CallTarget::Custom {
                method,
                url_arg_index,
                ..
            } => (PyHttpClientKind::Custom, *method, *url_arg_index),
        };

        let (line, snippet) = self.line_and_snippet(stmt, call_rel_offset);

        // Find the URL argument (`url=...` kwarg or positional index `url_arg_index`).
        let url_arg = find_url_arg(args, url_arg_index);

        // Scan all other arguments (e.g. `json={"name": customer["fullName"]}`) for field reads.
        for arg in args {
            let is_url_arg = url_arg.is_some_and(|u| std::ptr::eq(u, arg));
            if !is_url_arg {
                self.eval_expr_binding(stmt, &arg.value, arg.offset);
            } else if is_single_fstring(arg.value.trim()) {
                // Also scan f-string interpolations inside the URL for any field reads (e.g. `customer["id"]`).
                self.scan_fstring_interpolations(stmt, arg.value.trim(), arg.offset);
            }
        }

        let Some(url_arg) = url_arg else {
            self.unresolved_calls.push(PyUnresolvedCall {
                client: client_kind,
                line,
                snippet,
                reason: "missing URL argument".to_string(),
            });
            return None;
        };

        match self.eval_url_expr(&url_arg.value) {
            UrlEval::Resolved(url_str) => {
                let request = self.request_evidence(
                    stmt,
                    args,
                    client_kind,
                    &url_str,
                    line,
                    url_arg_index,
                    matches!(target, CallTarget::ClientInstance { .. }),
                );
                let matched_result = match &target {
                    CallTarget::Requests { .. } | CallTarget::Httpx { .. } => {
                        resolve_http_service_url(&url_str, self.ctx.services)
                    }
                    CallTarget::ClientInstance { base_url, .. } => {
                        let base = match base_url {
                            Some(UrlEval::Resolved(base)) => Ok(base.as_str()),
                            Some(UrlEval::Dynamic(reason)) => Err(reason.as_str()),
                            None => Err("relative URL on HTTP client without static base_url"),
                        };
                        resolve_url_with_base(&url_str, base, self.ctx.services)
                    }
                    CallTarget::Custom {
                        target_service,
                        path_prefix,
                        ..
                    } => resolve_custom_client_url(
                        &url_str,
                        target_service,
                        path_prefix.as_deref(),
                        self.ctx.services,
                    ),
                };

                match matched_result {
                    Ok(Some(matched)) => {
                        let id = self.calls.len();
                        self.calls.push(PyHttpCall {
                            id,
                            client: client_kind,
                            service: matched.service,
                            method,
                            normalized_path: matched.normalized_path,
                            request,
                            line,
                            snippet,
                        });
                        Some(match client_kind {
                            PyHttpClientKind::Requests | PyHttpClientKind::Httpx => {
                                VarBinding::HttpResponse(id)
                            }
                            PyHttpClientKind::Custom => VarBinding::JsonPayload {
                                call_id: id,
                                prefix: String::new(),
                            },
                        })
                    }
                    Ok(None) => None,
                    Err(reason) => {
                        self.unresolved_calls.push(PyUnresolvedCall {
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
                self.unresolved_calls.push(PyUnresolvedCall {
                    client: client_kind,
                    line,
                    snippet,
                    reason,
                });
                None
            }
        }
    }

    // Resuelve las URL estáticas y describe las dinámicas sin copiar su expresión.
    fn eval_url_expr(&self, raw_expr: &str) -> UrlEval {
        let (expr, _) = unwrap_await_and_parens(raw_expr, 0);
        if expr.is_empty() {
            return UrlEval::Dynamic("empty URL expression".to_string());
        }

        // 1. Static string literal
        if let Some(s) = parse_python_string_literal(expr) {
            return UrlEval::Resolved(s);
        }

        // 2. Single f-string literal
        if is_single_fstring(expr) {
            return self.eval_fstring_url(expr);
        }

        // 3. `os.getenv(...)` / `os.environ.get(...)` / `os.environ["..."]`
        if let Some(env_eval) = self.try_eval_env_expr(expr) {
            return env_eval;
        }

        // 4. Local identifier
        if is_simple_ident(expr) {
            return match self.lookup_var(expr) {
                Some(VarBinding::StringLiteral(val)) => UrlEval::Resolved(val.clone()),
                Some(VarBinding::DynamicString(reason)) => UrlEval::Dynamic(reason.clone()),
                _ => UrlEval::Dynamic(format!(
                    "dynamic URL variable `{expr}` cannot be resolved statically"
                )),
            };
        }

        // 5. String concatenation with `+`
        if let Some((left, right)) = split_top_level_plus(expr) {
            let l_eval = self.eval_url_expr(left);
            let r_eval = self.eval_url_expr(right);
            return match (l_eval, r_eval) {
                (UrlEval::Resolved(l), UrlEval::Resolved(r)) => {
                    UrlEval::Resolved(format!("{l}{r}"))
                }
                (UrlEval::Resolved(l), UrlEval::Dynamic(_)) if l.ends_with('/') => {
                    let r_trim = right.trim();
                    if is_simple_ident(r_trim) {
                        UrlEval::Resolved(format!("{l}:{r_trim}"))
                    } else {
                        UrlEval::Resolved(format!("{l}:param"))
                    }
                }
                (UrlEval::Dynamic(reason), _) => UrlEval::Dynamic(reason),
                _ => UrlEval::Dynamic(
                    "dynamic URL concatenation cannot be resolved statically".to_string(),
                ),
            };
        }

        UrlEval::Dynamic("dynamic URL expression cannot be resolved statically".into())
    }

    fn eval_fstring_url(&self, fstr: &str) -> UrlEval {
        let trimmed = fstr.trim();
        let bytes = trimmed.as_bytes();
        let Some((qpos, is_triple, quote)) = detect_string_start(bytes, 0) else {
            return UrlEval::Dynamic("invalid f-string literal".to_string());
        };
        if is_triple || trimmed.len() <= qpos + 1 || bytes[trimmed.len() - 1] != quote {
            return UrlEval::Dynamic("invalid f-string literal".to_string());
        }
        let is_raw = trimmed[..qpos]
            .bytes()
            .any(|b| b.eq_ignore_ascii_case(&b'r'));
        let inner = &trimmed[qpos + 1..trimmed.len() - 1];
        let inner_bytes = inner.as_bytes();

        let mut out = String::new();
        let mut i = 0usize;
        let mut lit_start = 0usize;

        while i < inner_bytes.len() {
            if inner_bytes[i] == b'\\' && i + 1 < inner_bytes.len() {
                i += 2;
                continue;
            }
            if inner_bytes[i] == b'{' {
                if inner_bytes.get(i + 1) == Some(&b'{') {
                    out.push_str(&unescape_python_string(&inner[lit_start..i], is_raw));
                    out.push('{');
                    i += 2;
                    lit_start = i;
                    continue;
                }
                out.push_str(&unescape_python_string(&inner[lit_start..i], is_raw));
                let Some(close_brace) = find_fstring_close_brace(inner_bytes, i) else {
                    return UrlEval::Dynamic("unterminated f-string interpolation".to_string());
                };
                let expr_raw = strip_fstring_format_spec(&inner[i + 1..close_brace]).trim();

                match self.eval_fstring_placeholder(expr_raw, out.is_empty()) {
                    Ok(segment) => out.push_str(&segment),
                    Err(reason) => return UrlEval::Dynamic(reason),
                }

                i = close_brace + 1;
                lit_start = i;
                continue;
            }
            if inner_bytes[i] == b'}' && inner_bytes.get(i + 1) == Some(&b'}') {
                out.push_str(&unescape_python_string(&inner[lit_start..i], is_raw));
                out.push('}');
                i += 2;
                lit_start = i;
                continue;
            }
            i += 1;
        }

        if lit_start < inner.len() {
            out.push_str(&unescape_python_string(&inner[lit_start..], is_raw));
        }

        UrlEval::Resolved(out)
    }

    fn eval_fstring_placeholder(&self, expr: &str, at_prefix: bool) -> Result<String, String> {
        if let Some(s) = parse_python_string_literal(expr) {
            return Ok(s);
        }

        if let Some(env_eval) = self.try_eval_env_expr(expr) {
            return match env_eval {
                UrlEval::Resolved(val) => Ok(val),
                UrlEval::Dynamic(reason) => {
                    if at_prefix {
                        Err(reason)
                    } else {
                        Ok(":param".to_string())
                    }
                }
            };
        }

        if is_simple_ident(expr) {
            return match self.lookup_var(expr) {
                Some(VarBinding::StringLiteral(val)) => Ok(val.clone()),
                Some(VarBinding::DynamicString(reason)) => {
                    if at_prefix {
                        Err(reason.clone())
                    } else {
                        Ok(format!(":{expr}"))
                    }
                }
                _ => {
                    if at_prefix {
                        Err(format!(
                            "dynamic URL prefix `{{{expr}}}` cannot be resolved statically"
                        ))
                    } else {
                        Ok(format!(":{expr}"))
                    }
                }
            };
        }

        if at_prefix {
            Err("dynamic f-string URL prefix cannot be resolved statically".to_string())
        } else {
            Ok(":param".to_string())
        }
    }

    // Resuelve el acceso a variables de entorno sin publicar argumentos literales.
    fn try_eval_env_expr(&self, expr: &str) -> Option<UrlEval> {
        let trimmed = expr.trim();

        // `os.getenv(...)` or `os.environ.get(...)`
        for prefix in ["os.getenv", "os.environ.get"] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let open = skip_ws(rest.as_bytes(), 0);
                if rest.as_bytes().get(open) == Some(&b'(')
                    && find_matching_delim(rest.as_bytes(), open, b'(', b')')
                        == Some(rest.len() - 1)
                {
                    let args = parse_call_args(&rest[open + 1..rest.len() - 1], 0);
                    return Some(self.eval_env_call_args(&args));
                }
            }
        }

        // `os.environ["KEY"]`
        if let Some(rest) = trimmed.strip_prefix("os.environ") {
            let open = skip_ws(rest.as_bytes(), 0);
            if rest.as_bytes().get(open) == Some(&b'[')
                && find_matching_delim(rest.as_bytes(), open, b'[', b']') == Some(rest.len() - 1)
            {
                let key_raw = &rest[open + 1..rest.len() - 1];
                if let Some(key) = parse_python_string_literal(key_raw) {
                    return Some(match self.ctx.env.get(&key) {
                        Some(val) => UrlEval::Resolved(val.clone()),
                        None => UrlEval::Dynamic(UNMAPPED_ENV.into()),
                    });
                }
                return Some(UrlEval::Dynamic(
                    "dynamic os.environ key cannot be resolved statically".to_string(),
                ));
            }
        }

        None
    }

    // Usa el valor configurado o por defecto y omite la clave literal en caso de fallo.
    fn eval_env_call_args(&self, args: &[CallArg]) -> UrlEval {
        let key_arg = args
            .iter()
            .find(|a| a.name.as_deref() == Some("key"))
            .or_else(|| args.iter().find(|a| a.name.is_none()));
        let Some(key_arg) = key_arg else {
            return UrlEval::Dynamic("os.getenv missing environment variable name".to_string());
        };
        let Some(key) = parse_python_string_literal(&key_arg.value) else {
            return UrlEval::Dynamic(
                "dynamic environment variable name cannot be resolved statically".to_string(),
            );
        };
        if let Some(val) = self.ctx.env.get(&key) {
            return UrlEval::Resolved(val.clone());
        }

        // Check for optional string literal default (`os.getenv("KEY", "http://default")`).
        let default_arg = args
            .iter()
            .find(|a| a.name.as_deref() == Some("default"))
            .or_else(|| args.iter().filter(|a| a.name.is_none()).nth(1));
        if let Some(def) = default_arg
            && let Some(val) = parse_python_string_literal(&def.value)
        {
            return UrlEval::Resolved(val);
        }

        UrlEval::Dynamic(UNMAPPED_ENV.into())
    }
}

/// Splits cleaned Python source into logical statements (joining multiline parenthesized
/// expressions and backslash continuations, and splitting top-level `;` statements).
fn split_logical_statements(cleaned: &str) -> Vec<LogicalStmt> {
    let bytes = cleaned.as_bytes();
    let mut out = Vec::new();

    let mut i = 0usize;
    let mut line = 1usize;

    let mut stmt_start = 0usize;
    let mut stmt_start_line = 1usize;
    let mut stmt_indent: Option<usize> = None;
    let mut paren_depth = 0usize;
    let mut line_start_pos = 0usize;

    let push_stmt = |out: &mut Vec<LogicalStmt>,
                     start: usize,
                     end: usize,
                     start_line: usize,
                     indent: Option<usize>| {
        let slice = &cleaned[start..end];
        if !slice.trim().is_empty() {
            out.push(LogicalStmt {
                indent: indent.unwrap_or(0),
                start_line,
                global_offset: start,
                text: slice.to_string(),
            });
        }
    };

    while i < bytes.len() {
        if stmt_indent.is_none() && !bytes[i].is_ascii_whitespace() {
            stmt_indent = Some(measure_indent(&cleaned[line_start_pos..i]));
            stmt_start = i;
            stmt_start_line = line;
        }

        if let Some(after_str) = try_skip_string_literal(bytes, i) {
            line += bytes[i..after_str].iter().filter(|&&b| b == b'\n').count();
            i = after_str;
            continue;
        }

        match bytes[i] {
            b'(' | b'[' | b'{' => {
                paren_depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'\n') => {
                line += 1;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'\r') && bytes.get(i + 2) == Some(&b'\n') => {
                line += 1;
                i += 3;
            }
            b';' if paren_depth == 0 => {
                push_stmt(&mut out, stmt_start, i, stmt_start_line, stmt_indent);
                i += 1;
                // Keep the same indent for subsequent semicolon-separated statements on this line!
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                    i += 1;
                }
                stmt_start = i;
                stmt_start_line = line;
            }
            b'\n' => {
                if paren_depth == 0 {
                    push_stmt(&mut out, stmt_start, i, stmt_start_line, stmt_indent);
                    stmt_indent = None;
                    stmt_start = i + 1;
                    stmt_start_line = line + 1;
                }
                line += 1;
                i += 1;
                line_start_pos = i;
            }
            _ => {
                i += 1;
            }
        }
    }

    if stmt_start < bytes.len() {
        push_stmt(
            &mut out,
            stmt_start,
            bytes.len(),
            stmt_start_line,
            stmt_indent,
        );
    }

    out
}

fn measure_indent(prefix: &str) -> usize {
    let mut cols = 0usize;
    for b in prefix.bytes() {
        match b {
            b' ' => cols += 1,
            b'\t' => cols += 4,
            _ => {}
        }
    }
    cols
}

struct ScopeHeader<'a> {
    name: Option<&'a str>,
    params: Vec<String>,
    inline_body: Option<(&'a str, usize)>,
}

// Identifica el nombre de una definición sin tomar su firma ni su cuerpo.
fn scope_header_name(rest: &str) -> Option<&str> {
    rest.trim_start()
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '(' | ':'))
        .next()
        .filter(|name| is_simple_ident(name))
}

// Extrae el nombre que se vincula en el ámbito exterior y los parámetros del nuevo ámbito.
fn parse_def_or_class_header(trimmed: &str) -> Option<ScopeHeader<'_>> {
    if let Some(rest_rel) =
        strip_keyword_prefix(trimmed, "async def").or_else(|| strip_keyword_prefix(trimmed, "def"))
    {
        let rest = &trimmed[rest_rel..];
        let (header, inline_body) = split_header_colon(rest);
        let bytes = header.as_bytes();
        let params = if let Some(open) = bytes.iter().position(|&b| b == b'(')
            && let Some(close) = find_matching_delim(bytes, open, b'(', b')')
        {
            parse_param_names(&header[open + 1..close])
        } else {
            Vec::new()
        };
        let inline_body = inline_body.map(|(b, rel)| (b, rest_rel + rel));
        return Some(ScopeHeader {
            name: scope_header_name(rest),
            params,
            inline_body,
        });
    }

    if let Some(rest_rel) = strip_keyword_prefix(trimmed, "class") {
        let rest = &trimmed[rest_rel..];
        let (_, inline_body) = split_header_colon(rest);
        let inline_body = inline_body.map(|(b, rel)| (b, rest_rel + rel));
        return Some(ScopeHeader {
            name: scope_header_name(rest),
            params: Vec::new(),
            inline_body,
        });
    }

    None
}

fn parse_param_names(raw: &str) -> Vec<String> {
    let mut params = Vec::new();
    for (part, _) in split_top_level_comma(raw) {
        let cleaned = part.trim().trim_start_matches('*');
        if let Some((name, _)) = read_ident(cleaned, 0)
            && name != "self"
            && name != "cls"
        {
            params.push(name.to_string());
        }
    }
    params
}

fn strip_keyword_prefix(trimmed: &str, kw: &str) -> Option<usize> {
    let rest = trimmed.strip_prefix(kw)?;
    let next = rest.as_bytes().first().copied()?;
    if next.is_ascii_whitespace() || next == b'(' {
        Some(kw.len())
    } else {
        None
    }
}

fn split_header_colon(rest: &str) -> (&str, Option<(&str, usize)>) {
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 => {
                let header = &rest[..i];
                let after = &rest[i + 1..];
                let after_trimmed = after.trim();
                let inline_body = if after_trimmed.is_empty() {
                    None
                } else {
                    let ws = after.len() - after.trim_start().len();
                    Some((after_trimmed, i + 1 + ws))
                };
                return (header, inline_body);
            }
            _ => {}
        }
        i += 1;
    }
    (rest, None)
}

fn split_with_as(item: &str) -> Option<(&str, &str)> {
    let pos = find_top_level_keyword(item, "as")?;
    let expr = item[..pos].trim();
    let var = item[pos + 2..].trim();
    Some((expr, var))
}

fn find_top_level_keyword(raw: &str, kw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let kw_bytes = kw.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && bytes[i..].starts_with(kw_bytes) => {
                let before_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
                let after_idx = i + kw_bytes.len();
                let after_ok = after_idx >= bytes.len() || !is_ident_continue(bytes[after_idx]);
                if before_ok && after_ok {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_assignment(trimmed: &str) -> Option<(&str, &str, usize)> {
    let bytes = trimmed.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'=' if depth == 0 => {
                let prev = i.checked_sub(1).map(|p| bytes[p]);
                let next = bytes.get(i + 1).copied();
                let is_non_assign = next == Some(b'=')
                    || matches!(
                        prev,
                        Some(
                            b'=' | b'!'
                                | b'<'
                                | b'>'
                                | b':'
                                | b'+'
                                | b'-'
                                | b'*'
                                | b'/'
                                | b'%'
                                | b'&'
                                | b'|'
                                | b'^'
                        )
                    );
                if !is_non_assign {
                    return Some((&trimmed[..i], &trimmed[i + 1..], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_type_annotation(lhs: &str) -> &str {
    let trimmed = lhs.trim();
    if let Some(colon) = find_top_level_byte(trimmed, b':') {
        &trimmed[..colon]
    } else {
        trimmed
    }
}

fn unwrap_await_and_parens(mut expr: &str, mut offset: usize) -> (&str, usize) {
    loop {
        let trimmed_start = expr.trim_start();
        offset += expr.len() - trimmed_start.len();
        expr = trimmed_start.trim_end();

        if let Some(rest_rel) = strip_keyword_prefix(expr, "await") {
            let after = &expr[rest_rel..];
            let ws = after.len() - after.trim_start().len();
            offset += rest_rel + ws;
            expr = after.trim();
            continue;
        }

        if expr.starts_with('(')
            && expr.ends_with(')')
            && find_matching_delim(expr.as_bytes(), 0, b'(', b')') == Some(expr.len() - 1)
            && !has_top_level_comma(&expr[1..expr.len() - 1])
        {
            expr = &expr[1..expr.len() - 1];
            offset += 1;
            continue;
        }

        break;
    }
    (expr, offset)
}

fn has_top_level_comma(raw: &str) -> bool {
    find_top_level_byte(raw, b',').is_some()
}

fn find_top_level_byte(raw: &str, needle: u8) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b if b == needle && depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_compound_expression(expr: &str) -> Option<Vec<(&str, usize)>> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut splits: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ if depth == 0 => {
                // Multi-char operators
                if bytes[i..].starts_with(b"==")
                    || bytes[i..].starts_with(b"!=")
                    || bytes[i..].starts_with(b"<=")
                    || bytes[i..].starts_with(b">=")
                    || bytes[i..].starts_with(b"//")
                    || bytes[i..].starts_with(b"**")
                    || bytes[i..].starts_with(b":=")
                {
                    splits.push((i, i + 2));
                    i += 2;
                    continue;
                }
                if matches!(
                    bytes[i],
                    b'+' | b'-' | b'*' | b'/' | b'%' | b',' | b'|' | b'&' | b'^' | b'<' | b'>'
                ) {
                    splits.push((i, i + 1));
                    i += 1;
                    continue;
                }
                // Keyword operators (`and`, `or`, `if`, `else`, `in`, `not`, `is`)
                let mut matched_kw = false;
                for kw in ["and", "or", "if", "else", "in", "not", "is"] {
                    let kw_b = kw.as_bytes();
                    if bytes[i..].starts_with(kw_b) {
                        let before_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
                        let after_idx = i + kw_b.len();
                        let after_ok =
                            after_idx >= bytes.len() || !is_ident_continue(bytes[after_idx]);
                        if before_ok && after_ok {
                            splits.push((i, after_idx));
                            i = after_idx;
                            matched_kw = true;
                            break;
                        }
                    }
                }
                if !matched_kw {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    if splits.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    let mut start = 0usize;
    for (op_start, op_end) in splits {
        let seg = &expr[start..op_start];
        let seg_trimmed = seg.trim();
        if !seg_trimmed.is_empty() {
            let ws = seg.len() - seg.trim_start().len();
            parts.push((seg_trimmed, start + ws));
        }
        start = op_end;
    }
    if start < expr.len() {
        let seg = &expr[start..];
        let seg_trimmed = seg.trim();
        if !seg_trimmed.is_empty() {
            let ws = seg.len() - seg.trim_start().len();
            parts.push((seg_trimmed, start + ws));
        }
    }

    Some(parts)
}

fn is_plus_only_chain(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut saw_plus = false;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'+' if depth == 0 => saw_plus = true,
            b'-' | b'*' | b'/' | b'%' | b',' | b'|' | b'&' | b'^' | b'<' | b'>' | b'='
                if depth == 0 =>
            {
                return false;
            }
            _ => {}
        }
        i += 1;
    }

    saw_plus
}

fn split_top_level_plus(expr: &str) -> Option<(&str, &str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    let mut last_plus = None;
    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'+' if depth == 0 => last_plus = Some(i),
            _ => {}
        }
        i += 1;
    }
    let pos = last_plus?;
    Some((&expr[..pos], &expr[pos + 1..]))
}

/// Parses `<primary>(.attr | (args) | [index])*` from a single non-compound expression.
fn parse_postfix_chain(expr: &str) -> Option<(&str, usize, Vec<PostfixOp>)> {
    let bytes = expr.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if i >= bytes.len() {
        return None;
    }

    let primary_start = i;
    let primary_end = if let Some(after_str) = try_skip_string_literal(bytes, i) {
        after_str
    } else if bytes[i] == b'(' {
        find_matching_delim(bytes, i, b'(', b')')? + 1
    } else if bytes[i] == b'[' {
        find_matching_delim(bytes, i, b'[', b']')? + 1
    } else if bytes[i] == b'{' {
        find_matching_delim(bytes, i, b'{', b'}')? + 1
    } else if is_ident_start(bytes[i]) {
        read_ident(expr, i)?.1
    } else {
        return None;
    };

    let primary = &expr[primary_start..primary_end];
    i = primary_end;
    let mut ops = Vec::new();

    while i < bytes.len() {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'.' => {
                let name_start = skip_ws(bytes, i + 1);
                let (name, after_name) = read_ident(expr, name_start)?;
                ops.push(PostfixOp::Attr {
                    name: name.to_string(),
                });
                i = after_name;
            }
            b'(' => {
                let open = i;
                let close = find_matching_delim(bytes, open, b'(', b')')?;
                ops.push(PostfixOp::Call {
                    raw_args: expr[open + 1..close].to_string(),
                    offset: open,
                    args_offset: open + 1,
                });
                i = close + 1;
            }
            b'[' => {
                let open = i;
                let close = find_matching_delim(bytes, open, b'[', b']')?;
                ops.push(PostfixOp::Subscript {
                    raw_index: expr[open + 1..close].to_string(),
                    offset: open,
                    index_offset: open + 1,
                });
                i = close + 1;
            }
            _ => return None,
        }
    }

    Some((primary, primary_start, ops))
}

fn is_single_fstring(raw: &str) -> bool {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    let Some((qpos, is_triple, quote)) = detect_string_start(bytes, 0) else {
        return false;
    };
    if is_triple
        || !trimmed[..qpos]
            .bytes()
            .any(|b| b.eq_ignore_ascii_case(&b'f'))
    {
        return false;
    }
    let end = scan_single_line_string(bytes, qpos, quote);
    end == bytes.len() && end > qpos + 1 && bytes[end - 1] == quote
}

fn extract_fstring_expressions(fstr: &str) -> Vec<(&str, usize)> {
    let bytes = fstr.as_bytes();
    let Some((qpos, is_triple, quote)) = detect_string_start(bytes, 0) else {
        return Vec::new();
    };
    if is_triple || fstr.len() <= qpos + 1 || bytes[fstr.len() - 1] != quote {
        return Vec::new();
    }

    let inner = &fstr[qpos + 1..fstr.len() - 1];
    let inner_bytes = inner.as_bytes();
    let mut exprs = Vec::new();
    let mut i = 0usize;

    while i < inner_bytes.len() {
        if inner_bytes[i] == b'\\' && i + 1 < inner_bytes.len() {
            i += 2;
            continue;
        }
        if inner_bytes[i] == b'{' {
            if inner_bytes.get(i + 1) == Some(&b'{') {
                i += 2;
                continue;
            }
            if let Some(close) = find_fstring_close_brace(inner_bytes, i) {
                let raw_inside = &inner[i + 1..close];
                let expr_part = strip_fstring_format_spec(raw_inside);
                let trimmed = expr_part.trim();
                if !trimmed.is_empty() {
                    let ws = expr_part.len() - expr_part.trim_start().len();
                    exprs.push((trimmed, qpos + 1 + i + 1 + ws));
                }
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }

    exprs
}

fn find_fstring_close_brace(bytes: &[u8], open_pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open_pos;
    while i < bytes.len() {
        if i > open_pos
            && let Some(after) = try_skip_string_literal(bytes, i)
        {
            i = after;
            continue;
        }
        match bytes[i] {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && bytes[i] == b'}' {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn strip_fstring_format_spec(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(after) = try_skip_string_literal(bytes, i) {
            i = after;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(i + 1) != Some(&b'=') => return &raw[..i],
            b'!' if depth == 0
                && matches!(bytes.get(i + 1), Some(b'r' | b's' | b'a'))
                && bytes.get(i + 2).is_none_or(|&b| b == b':') =>
            {
                return &raw[..i];
            }
            _ => {}
        }
        i += 1;
    }
    raw
}

fn extract_method_kwarg(args: &[CallArg]) -> Option<HttpMethod> {
    let arg = args.iter().find(|a| a.name.as_deref() == Some("method"))?;
    let raw = parse_python_string_literal(&arg.value)?;
    HttpMethod::parse(&raw)
}

fn extract_request_method_and_url_idx(args: &[CallArg], method_name: &str) -> (HttpMethod, usize) {
    if let Some(m) = extract_method_kwarg(args) {
        return (m, 0);
    }
    if method_name == "request" {
        let positional: Vec<&CallArg> = args.iter().filter(|a| a.name.is_none()).collect();
        if let Some(first) = positional.first()
            && let Some(raw_m) = parse_python_string_literal(&first.value)
            && let Some(m) = HttpMethod::parse(&raw_m)
        {
            return (m, 1);
        }
    }
    (HttpMethod::Get, 0)
}

fn find_url_arg(args: &[CallArg], pos_index: usize) -> Option<&CallArg> {
    if let Some(kw) = args
        .iter()
        .find(|a| matches!(a.name.as_deref(), Some("url" | "path" | "endpoint")))
    {
        return Some(kw);
    }
    args.iter().filter(|a| a.name.is_none()).nth(pos_index)
}

fn is_simple_ident(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    !bytes.is_empty()
        && is_ident_start(bytes[0])
        && bytes[1..].iter().copied().all(is_ident_continue)
}

// Classify static Python scalars and collections without retaining their values.
fn python_argument(value: &str, line: usize, wire: bool) -> Argument {
    if let Some(text) = parse_python_string_literal(value)
        && detect_string_start(value.as_bytes(), 0)
            .is_some_and(|(quote, _, _)| !value[..quote].to_ascii_lowercase().contains('b'))
    {
        return if wire {
            Argument::wire_text(line, &text)
        } else {
            Argument::new(line, Some(ArgumentType::String))
        };
    }
    if wire {
        return match value {
            "True" => Argument::wire_text(line, "True"),
            "False" => Argument::wire_text(line, "False"),
            value
                if is_integer_literal(value) || value.parse::<f64>().is_ok_and(f64::is_finite) =>
            {
                Argument::wire_text(line, value)
            }
            _ => Argument::new(line, None),
        };
    }
    let kind = match value {
        "None" => Some(ArgumentType::Null),
        "True" | "False" => Some(ArgumentType::Boolean),
        _ if is_integer_literal(value) => Some(ArgumentType::Integer),
        _ if value.parse::<f64>().is_ok_and(f64::is_finite) => Some(ArgumentType::Number),
        _ if value.starts_with('{')
            && find_matching_delim(value.as_bytes(), 0, b'{', b'}')
                == value.len().checked_sub(1)
            && (value[1..value.len() - 1].trim().is_empty()
                || split_top_level_comma(&value[1..value.len() - 1])
                    .iter()
                    .any(|(entry, _)| {
                        entry.trim_start().starts_with("**")
                            || python_dict_entry(entry.trim_start()).is_some()
                    })) =>
        {
            Some(ArgumentType::Object)
        }
        _ if value.starts_with('[')
            && find_matching_delim(value.as_bytes(), 0, b'[', b']')
                == value.len().checked_sub(1) =>
        {
            Some(ArgumentType::Array)
        }
        _ => None,
    };
    Argument::new(line, kind)
}

// Return a static string key and the offset of its dictionary value.
fn python_dict_entry(entry: &str) -> Option<(String, &str, usize)> {
    let bytes = entry.as_bytes();
    let mut pos = 0;
    while let Some(&byte) = bytes.get(pos) {
        if let Some(next) = try_skip_string_literal(bytes, pos) {
            pos = next;
        } else if byte == b':' {
            let (quote, _, _) = detect_string_start(bytes, 0)?;
            if entry[..quote]
                .bytes()
                .any(|byte| byte.eq_ignore_ascii_case(&b'b'))
            {
                return None;
            }
            let key = parse_python_string_literal(&entry[..pos])?;
            let start = skip_ws(bytes, pos + 1);
            return Some((key, &entry[start..], start));
        } else if byte.is_ascii_whitespace() {
            pos += 1;
        } else {
            return None;
        }
    }
    None
}

fn is_integer_literal(raw: &str) -> bool {
    let s = raw.strip_prefix('-').unwrap_or(raw);
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn join_field_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn sample_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            name: "customer-service".into(),
            path: PathBuf::from("./customer-service"),
            openapi: Some(PathBuf::from("openapi.yaml")),
            routes: None,
            base_urls: vec![
                "http://customer-service".into(),
                "https://api.internal".into(),
            ],
        }]
    }

    // Cover static scalar types and ambiguous or unsupported expressions.
    #[test]
    fn classifies_python_request_arguments() {
        use ArgumentType::*;
        for (value, wire, types) in [
            ("None", false, vec![Null]),
            ("True", false, vec![Boolean]),
            ("-42", false, vec![Integer]),
            ("2.5e1", false, vec![Number]),
            ("'hello'", false, vec![String]),
            ("'TRUE'", true, vec![String, Boolean]),
            ("42", true, vec![String, Integer]),
            ("[]", false, vec![Array]),
            ("{'key': thing}", false, vec![Object]),
            ("{**kwargs}", false, vec![Object]),
            ("{thing}", false, vec![]),
            ("b'hello'", false, vec![]),
            ("f'{value}'", true, vec![]),
            ("value", false, vec![]),
        ] {
            assert_eq!(python_argument(value, 9, wire), Argument { line: 9, types });
        }
    }

    // Verify quoted keys, source offsets, and unknown dictionary entry forms.
    #[test]
    fn extracts_only_static_string_keys_from_python_dicts() {
        assert_eq!(
            python_dict_entry("'a:b' :  current"),
            Some(("a:b".into(), "current", 9))
        );
        assert_eq!(
            python_dict_entry(r#""a" 'b': 1"#),
            Some(("ab".into(), "1", 9))
        );
        for entry in [
            "**kwargs",
            "key: 1",
            "'key' + suffix: 1",
            "f'{key}': 1",
            "b'key': 1",
        ] {
            assert_eq!(python_dict_entry(entry), None);
        }
    }

    // Confirm that kwargs and query collisions preserve unverified argument evidence.
    #[test]
    fn extracts_keyword_argument_evidence_from_python_clients() {
        use laine_core::request::ArgumentCompatibility::{Compatible, Unknown};
        let py = r#"import requests
import httpx
requests.post('http://customer-service/orders?limit=3',
              json={'lost': 'value', **data, 'kept': True},
              params={'limit': 2}, headers={'X-MODE': 'fast'})
requests.post('http://customer-service/orders?limit=3', **kwargs)
httpx.get('http://customer-service/orders?limit=3', params={'page': 1})
"#;
        let result = extract_python_file(
            "app.py",
            py,
            PyExtractContext {
                services: &sample_services(),
                clients: &[],
                env: &BTreeMap::new(),
            },
        );
        let [explicit, opaque, httpx] = result.calls.as_slice() else {
            panic!("unexpected calls: {:?}", result.calls);
        };
        assert_eq!(
            explicit
                .request
                .body
                .compare(Some("lost"), true, "string", explicit.line),
            Unknown(Some(4))
        );
        assert_eq!(
            explicit
                .request
                .body
                .compare(Some("kept"), true, "boolean", explicit.line),
            Compatible
        );
        assert_eq!(
            explicit
                .request
                .query
                .compare(Some("limit"), true, "integer", explicit.line),
            Unknown(Some(5))
        );
        assert_eq!(
            explicit
                .request
                .headers
                .compare(Some("x-mode"), true, "string", explicit.line),
            Compatible
        );
        assert_eq!(
            opaque
                .request
                .body
                .compare(Some("kept"), true, "boolean", opaque.line),
            Unknown(None)
        );
        assert_eq!(
            opaque
                .request
                .query
                .compare(Some("limit"), true, "integer", opaque.line),
            Unknown(Some(6))
        );
        assert_eq!(
            httpx
                .request
                .query
                .compare(Some("limit"), true, "integer", httpx.line),
            Unknown(Some(7))
        );
        assert_eq!(
            httpx
                .request
                .query
                .compare(Some("page"), true, "integer", httpx.line),
            Compatible
        );
    }

    // Verifica aliases de módulo, métodos y constructores con ocultamiento de nombres.
    #[test]
    fn resolves_http_import_aliases_in_their_scope() {
        let py = r#"import requests as rq, httpx as hx
from requests import get as get_url
from httpx import (
    AsyncClient as Client,
    post as send,
)

rq.get('http://customer-service/module')
hx.request('PATCH', 'http://customer-service/request')
get_url('http://customer-service/function')
send('http://customer-service/post')

async def use_client():
    async with Client(base_url='http://customer-service') as client:
        await client.get('/instance')
    import requests as local
    local.get('http://customer-service/local')

def shadow(rq):
    rq.get('http://customer-service/ignored')
    get_url('http://customer-service/outer')
    import other as get_url
    get_url('http://customer-service/also-ignored')

local.get('http://customer-service/outside-scope')
def rq():
    return None
rq.get('http://customer-service/shadowed-definition')
"#;
        let services = sample_services();
        let output = extract_python_file(
            "app.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &BTreeMap::new(),
            },
        );
        let hits = output
            .calls
            .iter()
            .map(|call| (call.normalized_path.as_str(), call.method))
            .collect::<Vec<_>>();
        assert_eq!(
            hits,
            [
                ("/module", HttpMethod::Get),
                ("/request", HttpMethod::Patch),
                ("/function", HttpMethod::Get),
                ("/post", HttpMethod::Post),
                ("/instance", HttpMethod::Get),
                ("/local", HttpMethod::Get),
                ("/outer", HttpMethod::Get),
            ]
        );
        assert!(output.unresolved_calls.is_empty());
    }

    #[test]
    fn extracts_requests_fstring_json_field_reads_chained_and_alias() {
        let py = r#"import requests

def fetch_customer(customer_id: str):
    res = requests.get(f"http://customer-service/customers/{customer_id}?expand=1")
    customer = res.json()
    name = customer["fullName"]
    email = customer.get("email")
    c = customer; nickname = c['nickname']
    return name, email, nickname

def fetch_chained(customer_id: str):
    return requests.get(f"http://customer-service/customers/{customer_id}").json()["fullName"]

def unrelated_scope():
    customer = {"fullName": "local-only"}
    return customer["fullName"]
"#;

        let services = sample_services();
        let env = BTreeMap::new();
        let extracted = extract_python_file(
            "src/billing.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &env,
            },
        );

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].client, PyHttpClientKind::Requests);
        assert_eq!(extracted.calls[0].service, "customer-service");
        assert_eq!(extracted.calls[0].method, HttpMethod::Get);
        assert_eq!(
            extracted.calls[0].normalized_path,
            "/customers/:customer_id"
        );
        assert_eq!(extracted.calls[0].line, 4);

        assert_eq!(extracted.calls[1].client, PyHttpClientKind::Requests);
        assert_eq!(extracted.calls[1].line, 12);
        assert_eq!(
            extracted.calls[1].normalized_path,
            "/customers/:customer_id"
        );

        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();
        assert_eq!(
            reads,
            vec![
                (0, "fullName", 6),
                (0, "email", 7),
                (0, "nickname", 8),
                (1, "fullName", 12),
            ]
        );
        assert!(extracted.unresolved_calls.is_empty());
    }

    #[test]
    fn extracts_httpx_client_base_url_and_async_client() {
        let py = r#"import httpx

def load_sync(customer_id: str):
    client = httpx.Client(base_url="http://customer-service")
    res = client.get(f"/customers/{customer_id}")
    data = res.json()
    return data["fullName"]

async def load_async(order_id: str):
    async with httpx.AsyncClient(base_url="https://api.internal") as client:
        res = await client.post(f"/orders/{order_id}")
        payload = res.json()
    return payload.get("status")
"#;

        let services = sample_services();
        let env = BTreeMap::new();
        let extracted = extract_python_file(
            "src/orders.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &env,
            },
        );

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].client, PyHttpClientKind::Httpx);
        assert_eq!(extracted.calls[0].service, "customer-service");
        assert_eq!(extracted.calls[0].method, HttpMethod::Get);
        assert_eq!(
            extracted.calls[0].normalized_path,
            "/customers/:customer_id"
        );
        assert_eq!(extracted.calls[0].line, 5);

        assert_eq!(extracted.calls[1].client, PyHttpClientKind::Httpx);
        assert_eq!(extracted.calls[1].service, "customer-service");
        assert_eq!(extracted.calls[1].method, HttpMethod::Post);
        assert_eq!(extracted.calls[1].normalized_path, "/orders/:order_id");
        assert_eq!(extracted.calls[1].line, 11);

        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();
        assert_eq!(reads, vec![(0, "fullName", 7), (1, "status", 13)]);
        assert!(extracted.unresolved_calls.is_empty());
    }

    #[test]
    // Verifica las variables ausentes sin publicar sus claves literales.
    fn resolves_os_getenv_and_os_environ_and_reports_missing_env_as_unresolved() {
        let py = r#"import os
import requests

BASE_URL = os.getenv("CUSTOMER_API_URL")

def mapped_calls(cid: str):
    r1 = requests.get(f"{BASE_URL}/customers/{cid}")
    r2 = requests.delete(f"{os.environ['CUSTOMER_API_URL']}/customers/{cid}")
    return r1.json()["fullName"], r2

def missing_env_calls():
    requests.get(os.getenv("MISSING_URL"))
    requests.post(f"{os.getenv('ALSO_MISSING')}/orders")
"#;

        let services = sample_services();
        let mut env = BTreeMap::new();
        env.insert(
            "CUSTOMER_API_URL".to_string(),
            "http://customer-service".to_string(),
        );

        let extracted = extract_python_file(
            "src/env_client.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &env,
            },
        );

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].method, HttpMethod::Get);
        assert_eq!(extracted.calls[0].normalized_path, "/customers/:cid");
        assert_eq!(extracted.calls[1].method, HttpMethod::Delete);
        assert_eq!(extracted.calls[1].normalized_path, "/customers/:cid");

        assert_eq!(extracted.field_reads.len(), 1);
        assert_eq!(extracted.field_reads[0].field_path, "fullName");

        assert_eq!(extracted.unresolved_calls.len(), 2);
        assert_eq!(extracted.unresolved_calls[0].line, 12);
        assert_eq!(extracted.unresolved_calls[0].reason, UNMAPPED_ENV);
        assert_eq!(extracted.unresolved_calls[1].line, 13);
        assert_eq!(extracted.unresolved_calls[1].reason, UNMAPPED_ENV);
    }

    #[test]
    fn extracts_configured_custom_client_wrappers() {
        let py = r#"def fetch_via_custom(customer_id: str):
    customer = api_client.get(f"/customers/{customer_id}")
    order = get_client().fetch("/orders", method="POST")
    return customer["fullName"], order.get("orderId")
"#;

        let services = sample_services();
        let clients = vec![
            CustomClientConfig {
                consumer_service: None,
                callee: "api_client".into(),
                target_service: "customer-service".into(),
                path_prefix: Some("/api".into()),
            },
            CustomClientConfig {
                consumer_service: None,
                callee: "get_client().fetch".into(),
                target_service: "customer-service".into(),
                path_prefix: None,
            },
        ];
        let env = BTreeMap::new();

        let extracted = extract_python_file(
            "src/custom.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &clients,
                env: &env,
            },
        );

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].client, PyHttpClientKind::Custom);
        assert_eq!(extracted.calls[0].service, "customer-service");
        assert_eq!(extracted.calls[0].method, HttpMethod::Get);
        assert_eq!(
            extracted.calls[0].normalized_path,
            "/api/customers/:customer_id"
        );

        assert_eq!(extracted.calls[1].client, PyHttpClientKind::Custom);
        assert_eq!(extracted.calls[1].method, HttpMethod::Post);
        assert_eq!(extracted.calls[1].normalized_path, "/orders");

        let reads: Vec<(usize, &str)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str()))
            .collect();
        assert_eq!(reads, vec![(0, "fullName"), (1, "orderId")]);
    }

    #[test]
    fn reports_dynamic_urls_as_unresolved_unknown_and_ignores_docstrings() {
        let py = r#"import requests
import httpx

"""
requests.get("http://customer-service/customers/ignored-docstring")
"""

# requests.get("http://customer-service/customers/ignored-comment")

def dynamic_calls(url: str):
    requests.get(build_url())
    httpx.post(url)
"#;

        let services = sample_services();
        let env = BTreeMap::new();
        let extracted = extract_python_file(
            "src/dynamic.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &env,
            },
        );

        assert!(extracted.calls.is_empty());
        assert_eq!(extracted.unresolved_calls.len(), 2);
        assert_eq!(
            extracted.unresolved_calls[0].client,
            PyHttpClientKind::Requests
        );
        assert_eq!(extracted.unresolved_calls[0].line, 11);
        assert_eq!(
            extracted.unresolved_calls[0].snippet,
            "requests.get(build_url())"
        );

        assert_eq!(
            extracted.unresolved_calls[1].client,
            PyHttpClientKind::Httpx
        );
        assert_eq!(extracted.unresolved_calls[1].line, 12);
        assert!(extracted.unresolved_calls[1].reason.contains("url"));
    }

    #[test]
    // Verifica los campos y el diagnóstico saneado de una URL base sin configurar.
    fn tracks_nested_fields_raise_for_status_concatenation_and_unmapped_httpx_base_url() {
        let py = r#"import os
import requests
import httpx

BASE_URL = "http://customer-service"

def inspect_customer(customer_id: str):
    res = requests.get(
        BASE_URL + "/customers/" + customer_id,
        timeout=5,
    )
    res.raise_for_status()
    customer = res.json()
    city = customer["address"]["city"]
    zip_code = customer.get("address", {}).get("zipCode")
    addr = customer["address"]
    street = addr["street"]
    return {"city": city, "zip": zip_code, "street": street, "name": customer["fullName"].strip()}

async def async_chained(cid: str):
    async with httpx.AsyncClient(base_url="http://customer-service") as client:
        return (await client.get(f"/customers/{cid}")).json()["email"]

def broken_httpx():
    client = httpx.Client(base_url=os.getenv("UNMAPPED_BASE"))
    return client.get("/customers/1")
"#;

        let services = sample_services();
        let env = BTreeMap::new();
        let extracted = extract_python_file(
            "src/nested.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &[],
                env: &env,
            },
        );

        assert_eq!(extracted.calls.len(), 2);
        assert_eq!(extracted.calls[0].line, 8);
        assert_eq!(
            extracted.calls[0].normalized_path,
            "/customers/:customer_id"
        );
        assert_eq!(extracted.calls[1].client, PyHttpClientKind::Httpx);
        assert_eq!(extracted.calls[1].line, 22);
        assert_eq!(extracted.calls[1].normalized_path, "/customers/:cid");

        let reads: Vec<(usize, &str, usize)> = extracted
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str(), r.line))
            .collect();
        assert_eq!(
            reads,
            vec![
                (0, "address.city", 14),
                (0, "address.zipCode", 15),
                (0, "address", 16),
                (0, "address.street", 17),
                (0, "fullName", 18),
                (1, "email", 22),
            ]
        );

        assert_eq!(extracted.unresolved_calls.len(), 1);
        assert_eq!(
            extracted.unresolved_calls[0].client,
            PyHttpClientKind::Httpx
        );
        assert_eq!(extracted.unresolved_calls[0].line, 26);
        assert_eq!(extracted.unresolved_calls[0].reason, UNMAPPED_ENV);
    }
}
