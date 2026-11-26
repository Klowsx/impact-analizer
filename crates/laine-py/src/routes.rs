use std::collections::{BTreeMap, HashSet};

use laine_core::model::{EndpointId, HttpMethod};
use laine_core::routes::{ExtractedRoute, normalize_route_path};

/// Extracts FastAPI HTTP routes from a Python module.
///
/// Supports:
/// - `@app.get/post/put/patch/delete("/customers/{id}")` (and any router variable name)
/// - `@app.api_route("/x", methods=["GET", "POST"])` (defaults to `GET` if `methods` is omitted)
/// - `router = APIRouter(prefix="/api/v1")` (and `fastapi.APIRouter(...)`)
/// - `app.include_router(router, prefix="/v2")` (combining `mount_prefix` + `include_router`
///   prefix + `APIRouter` prefix + route path, regardless of declaration order in the file)
/// - Path parameter normalization (`{id}` and `{id:int}` -> `:id`) via
///   [`laine_core::routes::normalize_route_path`]
/// - Skipping `#` comments and triple-quoted docstrings while preserving exact 1-based line numbers
pub fn extract_fastapi_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Vec<ExtractedRoute> {
    extract_python_routes(
        service,
        file_path,
        source,
        mount_prefix,
        PyRouteStyle::FastApi,
    )
}

/// Extrae las rutas HTTP de Flask de un módulo Python.
///
/// Supports:
/// - `@app.route("/customers/<int:id>", methods=["GET", "POST"])` (defaults to `GET` when
///   `methods` is omitted)
/// - `@app.get/post/put/patch/delete(...)` (and blueprint variables like `@bp.route(...)`)
/// - `bp = Blueprint("name", __name__, url_prefix="/api")` (and `flask.Blueprint(...)`)
/// - `app.register_blueprint(bp, url_prefix="/v1")` overrides the `Blueprint` default prefix.
///   Prefixes from parent blueprints and `mount_prefix` still apply.
/// - Path parameter normalization (`<int:id>`, `<string:slug>`, `<id>` -> `:id`, `:slug`) via
///   [`laine_core::routes::normalize_route_path`]
/// - Skipping `#` comments and triple-quoted docstrings while preserving exact 1-based line numbers
pub fn extract_flask_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Vec<ExtractedRoute> {
    extract_python_routes(
        service,
        file_path,
        source,
        mount_prefix,
        PyRouteStyle::Flask,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PyRouteStyle {
    FastApi,
    Flask,
}

#[derive(Debug, Clone)]
struct MountEntry {
    parent: String,
    prefix: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingDecorator {
    line: usize,
    target: String,
    methods: Vec<HttpMethod>,
    raw_path: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CallArg {
    pub name: Option<String>,
    pub value: String,
    pub offset: usize,
}

// Extrae decoradores y montajes y compone las rutas según el framework.
fn extract_python_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
    style: PyRouteStyle,
) -> Vec<ExtractedRoute> {
    let cleaned = strip_python_comments_and_docstrings(source);
    let raw_lines: Vec<&str> = source.lines().collect();

    let mut router_prefixes: BTreeMap<String, String> = BTreeMap::new();
    let mut mounts: BTreeMap<String, Vec<MountEntry>> = BTreeMap::new();
    let mut decorators: Vec<PendingDecorator> = Vec::new();

    let bytes = cleaned.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut at_line_start = true;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line += 1;
            at_line_start = true;
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Skip string literals so route-like text inside strings is never misinterpreted.
        if let Some(after_str) = try_skip_string_literal(bytes, i) {
            line += count_newlines(&bytes[i..after_str]);
            at_line_start = false;
            i = after_str;
            continue;
        }

        // 1. Decorator: `@<target>.<method>(...)`
        if at_line_start && b == b'@' {
            if let Some((dec, next_i)) = parse_route_decorator(&cleaned, i, line, style) {
                decorators.push(dec);
                line += count_newlines(&bytes[i..next_i]);
                at_line_start = false;
                i = next_i;
                continue;
            }
            at_line_start = false;
            i += 1;
            continue;
        }

        // 2. Statement-level router/blueprint constructor or mount call
        if at_line_start && is_ident_start(b) {
            if let Some((var_name, prefix, next_i)) = try_parse_router_ctor(&cleaned, i, style) {
                if let Some(p) = prefix {
                    router_prefixes.insert(var_name, p);
                }
                line += count_newlines(&bytes[i..next_i]);
                at_line_start = false;
                i = next_i;
                continue;
            }

            if let Some((parent, child, prefix, next_i)) = try_parse_mount_call(&cleaned, i, style)
            {
                mounts
                    .entry(child)
                    .or_default()
                    .push(MountEntry { parent, prefix });
                line += count_newlines(&bytes[i..next_i]);
                at_line_start = false;
                i = next_i;
                continue;
            }
        }

        at_line_start = false;
        i += 1;
    }

    let mut routes = Vec::new();
    for dec in decorators {
        let chains =
            resolve_prefix_chains(&dec.target, mount_prefix, &router_prefixes, &mounts, style);
        let snippet = raw_lines
            .get(dec.line.saturating_sub(1))
            .map(|l| l.trim().to_string())
            .unwrap_or_default();

        for chain in chains {
            let prefix_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
            let normalized_path = match style {
                PyRouteStyle::FastApi => normalize_fastapi_path(&prefix_refs, &dec.raw_path),
                PyRouteStyle::Flask => normalize_flask_path(&prefix_refs, &dec.raw_path),
            };

            for method in &dec.methods {
                routes.push(ExtractedRoute {
                    endpoint: EndpointId {
                        service: service.to_string(),
                        method: *method,
                        normalized_path: normalized_path.clone(),
                    },
                    file: file_path.to_string(),
                    line: dec.line,
                    snippet: snippet.clone(),
                });
            }
        }
    }

    routes
}

fn parse_route_decorator(
    cleaned: &str,
    at_pos: usize,
    line: usize,
    style: PyRouteStyle,
) -> Option<(PendingDecorator, usize)> {
    let bytes = cleaned.as_bytes();
    let (chain, after_chain) = read_dotted_ident(cleaned, skip_inline_ws(bytes, at_pos + 1))?;
    let (target, method_name) = chain.rsplit_once('.')?;
    let open_paren = skip_ws(bytes, after_chain);
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }
    let close_paren = find_matching_delim(bytes, open_paren, b'(', b')')?;
    let args = parse_call_args(&cleaned[open_paren + 1..close_paren], open_paren + 1);

    let methods = match style {
        PyRouteStyle::FastApi => {
            if let Some(m) = HttpMethod::parse(method_name) {
                vec![m]
            } else if method_name == "api_route" {
                let parsed = extract_methods_kwarg(&args);
                if parsed.is_empty() {
                    vec![HttpMethod::Get]
                } else {
                    parsed
                }
            } else {
                return None;
            }
        }
        PyRouteStyle::Flask => {
            if let Some(m) = HttpMethod::parse(method_name) {
                vec![m]
            } else if method_name == "route" {
                let parsed = extract_methods_kwarg(&args);
                if parsed.is_empty() {
                    vec![HttpMethod::Get]
                } else {
                    parsed
                }
            } else {
                return None;
            }
        }
    };

    let kwarg_name = match style {
        PyRouteStyle::FastApi => "path",
        PyRouteStyle::Flask => "rule",
    };

    let raw_path = extract_string_arg(&args, 0, kwarg_name)?;

    Some((
        PendingDecorator {
            line,
            target: target.to_string(),
            methods,
            raw_path,
        },
        close_paren + 1,
    ))
}

fn try_parse_router_ctor(
    cleaned: &str,
    start: usize,
    style: PyRouteStyle,
) -> Option<(String, Option<String>, usize)> {
    let bytes = cleaned.as_bytes();
    let (var_name, mut pos) = read_ident(cleaned, start)?;
    pos = skip_inline_ws(bytes, pos);

    // Optional type annotation: `router: APIRouter = APIRouter(...)`
    if bytes.get(pos) == Some(&b':') {
        pos += 1;
        while pos < bytes.len() && bytes[pos] != b'=' && bytes[pos] != b'\n' {
            pos += 1;
        }
    }

    if bytes.get(pos) != Some(&b'=') || bytes.get(pos + 1) == Some(&b'=') {
        return None;
    }
    pos = skip_inline_ws(bytes, pos + 1);

    let (ctor_chain, after_ctor) = read_dotted_ident(cleaned, pos)?;
    let ctor_name = ctor_chain.rsplit('.').next().unwrap_or(ctor_chain);
    let (expected_ctor, prefix_kwarg) = match style {
        PyRouteStyle::FastApi => ("APIRouter", "prefix"),
        PyRouteStyle::Flask => ("Blueprint", "url_prefix"),
    };
    if ctor_name != expected_ctor {
        return None;
    }

    let open_paren = skip_ws(bytes, after_ctor);
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }
    let close_paren = find_matching_delim(bytes, open_paren, b'(', b')')?;
    let args = parse_call_args(&cleaned[open_paren + 1..close_paren], open_paren + 1);
    let prefix = extract_kwarg_string(&args, prefix_kwarg);

    Some((var_name.to_string(), prefix, close_paren + 1))
}

fn try_parse_mount_call(
    cleaned: &str,
    start: usize,
    style: PyRouteStyle,
) -> Option<(String, String, Option<String>, usize)> {
    let bytes = cleaned.as_bytes();
    let (chain, after_chain) = read_dotted_ident(cleaned, start)?;
    let (parent, method_name) = chain.rsplit_once('.')?;

    let (expected_method, target_kwarg, prefix_kwarg) = match style {
        PyRouteStyle::FastApi => ("include_router", "router", "prefix"),
        PyRouteStyle::Flask => ("register_blueprint", "blueprint", "url_prefix"),
    };
    if method_name != expected_method {
        return None;
    }

    let open_paren = skip_ws(bytes, after_chain);
    if bytes.get(open_paren) != Some(&b'(') {
        return None;
    }
    let close_paren = find_matching_delim(bytes, open_paren, b'(', b')')?;
    let args = parse_call_args(&cleaned[open_paren + 1..close_paren], open_paren + 1);

    let child_raw = args
        .iter()
        .find(|a| a.name.as_deref() == Some(target_kwarg))
        .or_else(|| args.iter().find(|a| a.name.is_none()))?
        .value
        .trim();

    if child_raw.is_empty()
        || !child_raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return None;
    }

    let prefix = extract_kwarg_string(&args, prefix_kwarg);
    Some((
        parent.to_string(),
        child_raw.to_string(),
        prefix,
        close_paren + 1,
    ))
}

// Reúne las cadenas de prefijos que llevan a un router o blueprint.
fn resolve_prefix_chains(
    target: &str,
    mount_prefix: Option<&str>,
    router_prefixes: &BTreeMap<String, String>,
    mounts: &BTreeMap<String, Vec<MountEntry>>,
    style: PyRouteStyle,
) -> Vec<Vec<String>> {
    let mut visited = HashSet::new();
    resolve_prefix_chains_inner(
        target,
        mount_prefix,
        router_prefixes,
        mounts,
        style,
        &mut visited,
    )
}

// Recorre los montajes padre y aplica la regla de prefijos de cada framework.
fn resolve_prefix_chains_inner(
    target: &str,
    mount_prefix: Option<&str>,
    router_prefixes: &BTreeMap<String, String>,
    mounts: &BTreeMap<String, Vec<MountEntry>>,
    style: PyRouteStyle,
    visited: &mut HashSet<String>,
) -> Vec<Vec<String>> {
    let own_prefix = router_prefixes.get(target);

    if !visited.insert(target.to_string()) {
        let mut chain = Vec::new();
        if let Some(mp) = mount_prefix {
            chain.push(mp.to_string());
        }
        if let Some(op) = own_prefix {
            chain.push(op.clone());
        }
        return vec![chain];
    }

    let result = if let Some(entries) = mounts.get(target) {
        let mut all_chains = Vec::new();
        for entry in entries {
            let parent_chains = resolve_prefix_chains_inner(
                &entry.parent,
                mount_prefix,
                router_prefixes,
                mounts,
                style,
                visited,
            );
            for mut chain in parent_chains {
                match style {
                    PyRouteStyle::FastApi => {
                        chain.extend(entry.prefix.iter().chain(own_prefix).cloned());
                    }
                    PyRouteStyle::Flask => {
                        // Incluso un prefijo vacío sustituye al valor del constructor.
                        chain.extend(entry.prefix.as_ref().or(own_prefix).cloned());
                    }
                }
                all_chains.push(chain);
            }
        }
        all_chains
    } else {
        let mut chain = Vec::new();
        if let Some(mp) = mount_prefix {
            chain.push(mp.to_string());
        }
        if let Some(op) = own_prefix {
            chain.push(op.clone());
        }
        vec![chain]
    };

    visited.remove(target);
    result
}

fn normalize_fastapi_path(prefixes: &[&str], raw_path: &str) -> String {
    let cleaned_prefixes: Vec<String> = prefixes
        .iter()
        .map(|p| preprocess_fastapi_segments(p))
        .collect();
    let cleaned_prefix_refs: Vec<&str> = cleaned_prefixes.iter().map(String::as_str).collect();
    let cleaned_raw = preprocess_fastapi_segments(raw_path);
    normalize_route_path(&cleaned_prefix_refs, &cleaned_raw)
}

fn preprocess_fastapi_segments(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            let trimmed = seg.trim();
            if trimmed.starts_with('{') && trimmed.ends_with('}') && trimmed.len() > 2 {
                let inner = &trimmed[1..trimmed.len() - 1];
                let name = inner.split(':').next().unwrap_or(inner).trim();
                format!("{{{name}}}")
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_flask_path(prefixes: &[&str], raw_path: &str) -> String {
    let cleaned_prefixes: Vec<String> = prefixes
        .iter()
        .map(|p| preprocess_flask_segments(p))
        .collect();
    let cleaned_prefix_refs: Vec<&str> = cleaned_prefixes.iter().map(String::as_str).collect();
    let cleaned_raw = preprocess_flask_segments(raw_path);
    normalize_route_path(&cleaned_prefix_refs, &cleaned_raw)
}

fn preprocess_flask_segments(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            let trimmed = seg.trim();
            if trimmed.starts_with('<') && trimmed.ends_with('>') && trimmed.len() > 2 {
                let inner = &trimmed[1..trimmed.len() - 1];
                let name = inner.rsplit(':').next().unwrap_or(inner).trim();
                format!("{{{name}}}")
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn extract_string_arg(args: &[CallArg], pos_index: usize, kwarg_name: &str) -> Option<String> {
    if let Some(kw) = args.iter().find(|a| a.name.as_deref() == Some(kwarg_name))
        && let Some(val) = parse_python_string_literal(&kw.value)
    {
        return Some(val);
    }

    let positional: Vec<&CallArg> = args.iter().filter(|a| a.name.is_none()).collect();
    positional
        .get(pos_index)
        .and_then(|arg| parse_python_string_literal(&arg.value))
}

fn extract_kwarg_string(args: &[CallArg], kwarg_name: &str) -> Option<String> {
    args.iter()
        .find(|a| a.name.as_deref() == Some(kwarg_name))
        .and_then(|a| parse_python_string_literal(&a.value))
}

fn extract_methods_kwarg(args: &[CallArg]) -> Vec<HttpMethod> {
    let Some(arg) = args.iter().find(|a| a.name.as_deref() == Some("methods")) else {
        return Vec::new();
    };
    let trimmed = arg.value.trim();
    let inner = if (trimmed.starts_with('[') && trimmed.ends_with(']'))
        || (trimmed.starts_with('(') && trimmed.ends_with(')'))
        || (trimmed.starts_with('{') && trimmed.ends_with('}'))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        return Vec::new();
    };

    let mut methods = Vec::new();
    for item in split_top_level_comma(inner) {
        if let Some(raw_method) = parse_python_string_literal(item.0)
            && let Some(method) = HttpMethod::parse(&raw_method)
            && !methods.contains(&method)
        {
            methods.push(method);
        }
    }
    methods
}

/// Strips Python `#` comments and triple-quoted strings (`"""..."""` / `'''...'''`)
/// while preserving exact 1-based line numbers and valid UTF-8.
pub(crate) fn strip_python_comments_and_docstrings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0usize;

    while i < bytes.len() {
        // Check for optional string prefix before quote (`r`, `u`, `b`, `f`, `rb`, `br`, `rf`, `fr`).
        if let Some((prefix_end, is_triple, quote)) = detect_string_start(bytes, i) {
            if is_triple {
                let mut j = prefix_end + 3;
                let mut newlines = 0usize;
                while j < bytes.len() {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        if bytes[j + 1] == b'\n' {
                            newlines += 1;
                        }
                        j += 2;
                        continue;
                    }
                    if j + 2 < bytes.len()
                        && bytes[j] == quote
                        && bytes[j + 1] == quote
                        && bytes[j + 2] == quote
                    {
                        j += 3;
                        break;
                    }
                    if bytes[j] == b'\n' {
                        newlines += 1;
                    }
                    j += 1;
                }
                out.push_str("\"\"");
                for _ in 0..newlines {
                    out.push('\n');
                }
                i = j;
                continue;
            } else {
                // Single-line string literal: preserve verbatim (including `#` inside quotes).
                let end = scan_single_line_string(bytes, prefix_end, quote);
                out.push_str(&source[i..end]);
                i = end;
                continue;
            }
        }

        if bytes[i] == b'#' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Copy UTF-8 character safely.
        let ch_len = utf8_char_len(bytes[i]).min(bytes.len() - i);
        out.push_str(&source[i..i + ch_len]);
        i += ch_len;
    }

    out
}

/// Detects whether `bytes[pos..]` starts a Python string literal (with optional prefix).
/// Returns `(quote_start_index, is_triple_quoted, quote_byte)`.
pub(crate) fn detect_string_start(bytes: &[u8], pos: usize) -> Option<(usize, bool, u8)> {
    let b = *bytes.get(pos)?;
    if b == b'\'' || b == b'"' {
        let is_triple = pos + 2 < bytes.len() && bytes[pos + 1] == b && bytes[pos + 2] == b;
        return Some((pos, is_triple, b));
    }

    // Only treat ASCII letters as a string prefix if not preceded by an identifier character.
    if pos > 0 && is_ident_continue(bytes[pos - 1]) {
        return None;
    }

    for prefix_len in [2usize, 1usize] {
        if pos + prefix_len < bytes.len() {
            let prefix = &bytes[pos..pos + prefix_len];
            let next = bytes[pos + prefix_len];
            if (next == b'\'' || next == b'"') && is_valid_py_string_prefix(prefix) {
                let qpos = pos + prefix_len;
                let is_triple =
                    qpos + 2 < bytes.len() && bytes[qpos + 1] == next && bytes[qpos + 2] == next;
                return Some((qpos, is_triple, next));
            }
        }
    }

    None
}

fn is_valid_py_string_prefix(prefix: &[u8]) -> bool {
    match prefix.len() {
        1 => matches!(prefix[0].to_ascii_lowercase(), b'r' | b'u' | b'b' | b'f'),
        2 => {
            let a = prefix[0].to_ascii_lowercase();
            let b = prefix[1].to_ascii_lowercase();
            matches!(
                (a, b),
                (b'r', b'f') | (b'f', b'r') | (b'r', b'b') | (b'b', b'r')
            )
        }
        _ => false,
    }
}

pub(crate) fn scan_single_line_string(bytes: &[u8], quote_pos: usize, quote: u8) -> usize {
    let mut i = quote_pos + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        if bytes[i] == b'\n' {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

pub(crate) fn try_skip_string_literal(bytes: &[u8], pos: usize) -> Option<usize> {
    let (qpos, is_triple, quote) = detect_string_start(bytes, pos)?;
    if is_triple {
        let mut j = qpos + 3;
        while j < bytes.len() {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
                continue;
            }
            if j + 2 < bytes.len()
                && bytes[j] == quote
                && bytes[j + 1] == quote
                && bytes[j + 2] == quote
            {
                return Some(j + 3);
            }
            j += 1;
        }
        Some(bytes.len())
    } else {
        Some(scan_single_line_string(bytes, qpos, quote))
    }
}

pub(crate) fn find_matching_delim(
    bytes: &[u8],
    open_pos: usize,
    open: u8,
    close: u8,
) -> Option<usize> {
    if bytes.get(open_pos) != Some(&open) {
        return None;
    }
    let mut depth = 0usize;
    let mut i = open_pos;
    while i < bytes.len() {
        if let Some(after_str) = try_skip_string_literal(bytes, i) {
            i = after_str;
            continue;
        }
        if bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn parse_call_args(raw_inside_parens: &str, base_offset: usize) -> Vec<CallArg> {
    let mut args = Vec::new();
    for (part, rel_offset) in split_top_level_comma(raw_inside_parens) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let leading_ws = part.len() - part.trim_start().len();
        let arg_offset = base_offset + rel_offset + leading_ws;

        if let Some((kw, val, val_rel)) = split_kwarg(trimmed) {
            args.push(CallArg {
                name: Some(kw.to_string()),
                value: val.to_string(),
                offset: arg_offset + val_rel,
            });
        } else {
            args.push(CallArg {
                name: None,
                value: trimmed.to_string(),
                offset: arg_offset,
            });
        }
    }
    args
}

pub(crate) fn split_top_level_comma(raw: &str) -> Vec<(&str, usize)> {
    let bytes = raw.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(after_str) = try_skip_string_literal(bytes, i) {
            i = after_str;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push((&raw[start..i], start));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    if start <= raw.len() {
        parts.push((&raw[start..], start));
    }
    parts
}

fn split_kwarg(raw: &str) -> Option<(&str, &str, usize)> {
    let bytes = raw.as_bytes();
    let (name, after_name) = read_ident(raw, 0)?;
    let eq_pos = skip_ws(bytes, after_name);
    if bytes.get(eq_pos) != Some(&b'=') || bytes.get(eq_pos + 1) == Some(&b'=') {
        return None;
    }
    let val_start = skip_ws(bytes, eq_pos + 1);
    let val = raw[val_start..].trim_end();
    Some((name, val, val_start))
}

/// Parses a static Python string literal (non-f-string), supporting adjacent implicit
/// string concatenation (e.g. `"/api" "/v1"` -> `"/api/v1"`).
pub(crate) fn parse_python_string_literal(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut result = String::new();
    let mut saw_any = false;

    while i < bytes.len() {
        i = skip_ws(bytes, i);
        if i >= bytes.len() {
            break;
        }

        let (qpos, is_triple, quote) = detect_string_start(bytes, i)?;
        if is_triple {
            return None;
        }
        let prefix = &trimmed[i..qpos];
        if prefix.bytes().any(|b| b.eq_ignore_ascii_case(&b'f')) {
            return None;
        }
        let is_raw = prefix.bytes().any(|b| b.eq_ignore_ascii_case(&b'r'));

        let end = scan_single_line_string(bytes, qpos, quote);
        if end <= qpos + 1 || bytes[end - 1] != quote {
            return None;
        }
        let inner = &trimmed[qpos + 1..end - 1];
        result.push_str(&unescape_python_string(inner, is_raw));
        saw_any = true;
        i = end;
    }

    saw_any.then_some(result)
}

pub(crate) fn unescape_python_string(inner: &str, is_raw: bool) -> String {
    if is_raw {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

pub(crate) fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) fn read_ident(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    if !bytes.get(start).copied().is_some_and(is_ident_start) {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() && is_ident_continue(bytes[i]) {
        i += 1;
    }
    Some((&source[start..i], i))
}

fn read_dotted_ident(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let (_, mut end) = read_ident(source, start)?;
    while bytes.get(end) == Some(&b'.') {
        let (_, next_end) = read_ident(source, end + 1)?;
        end = next_end;
    }
    Some((&source[start..end], end))
}

fn skip_inline_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\r') {
        i += 1;
    }
    i
}

pub(crate) fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn count_newlines(slice: &[u8]) -> usize {
    slice.iter().filter(|&&b| b == b'\n').count()
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte & 0xE0 == 0xC0 {
        2
    } else if first_byte & 0xF0 == 0xE0 {
        3
    } else if first_byte & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_fastapi_routes_with_api_router_include_router_comments_and_docstrings() {
        let py = r#"from fastapi import APIRouter, FastAPI

app = FastAPI()

# @app.get("/commented-out")
"""
Example docstring:
@app.post("/inside-docstring")
"""

router = APIRouter(prefix="/api/v1")

@app.get("/health")
def health():
    return {"ok": True}

@router.get("/customers/{id:int}")
def get_customer(id: int):
    '''
    @router.delete("/inside-single-docstring")
    '''
    return {}

@router.post("/orders")
def create_order():
    return {}

@app.put("/customers/{id}")
def update_customer(id: str):
    return {}

@app.patch("/customers/{id}")
def patch_customer(id: str):
    return {}

@app.delete("/customers/{id}")
def delete_customer(id: str):
    return {}

@app.api_route("/metrics", methods=["GET", "POST"])
def metrics():
    return {}

app.include_router(router, prefix="/v2")
"#;

        let routes = extract_fastapi_routes("customer-service", "app/main.py", py, Some("/svc"));
        let summary: Vec<(HttpMethod, &str, usize)> = routes
            .iter()
            .map(|r| {
                (
                    r.endpoint.method,
                    r.endpoint.normalized_path.as_str(),
                    r.line,
                )
            })
            .collect();

        assert_eq!(
            summary,
            vec![
                (HttpMethod::Get, "/svc/health", 13),
                (HttpMethod::Get, "/svc/v2/api/v1/customers/:id", 17),
                (HttpMethod::Post, "/svc/v2/api/v1/orders", 24),
                (HttpMethod::Put, "/svc/customers/:id", 28),
                (HttpMethod::Patch, "/svc/customers/:id", 32),
                (HttpMethod::Delete, "/svc/customers/:id", 36),
                (HttpMethod::Get, "/svc/metrics", 40),
                (HttpMethod::Post, "/svc/metrics", 40),
            ]
        );
        assert_eq!(routes[1].snippet, r#"@router.get("/customers/{id:int}")"#);
        assert_eq!(routes[0].endpoint.service, "customer-service");
        assert_eq!(routes[0].file, "app/main.py");
    }

    #[test]
    fn extracts_fastapi_standalone_router_without_include_router() {
        let py = r#"from fastapi import APIRouter

router = APIRouter(prefix="/customers")

@router.get("")
def list_customers():
    return []

@router.get("/{customer_id}")
def get_customer(customer_id: str):
    return {}
"#;

        let routes = extract_fastapi_routes("customer-service", "routers/customers.py", py, None);
        let summary: Vec<(HttpMethod, &str)> = routes
            .iter()
            .map(|r| (r.endpoint.method, r.endpoint.normalized_path.as_str()))
            .collect();

        assert_eq!(
            summary,
            vec![
                (HttpMethod::Get, "/customers"),
                (HttpMethod::Get, "/customers/:customer_id"),
            ]
        );
    }

    // Verifica que el prefijo del registro sustituye al del constructor del blueprint.
    #[test]
    fn extracts_flask_routes_with_blueprint_converters_methods_and_docstrings() {
        let py = r#"from flask import Blueprint, Flask

app = Flask(__name__)

# @app.route("/ignored")
"""
@bp.route("/also-ignored")
"""

bp = Blueprint("customers", __name__, url_prefix="/api")

@app.route("/health")
def health():
    return "ok"

@bp.route("/customers/<int:id>", methods=["GET", "POST"])
def customer_detail(id):
    return {}

@bp.get("/posts/<string:slug>")
def post_by_slug(slug):
    return {}

@bp.delete("/items/<id>")
def delete_item(id):
    return {}

@app.put("/settings")
def update_settings():
    return {}

@app.patch("/settings")
def patch_settings():
    return {}

app.register_blueprint(bp, url_prefix="/v1")
"#;

        let routes = extract_flask_routes("customer-service", "app.py", py, None);
        let summary: Vec<(HttpMethod, &str, usize)> = routes
            .iter()
            .map(|r| {
                (
                    r.endpoint.method,
                    r.endpoint.normalized_path.as_str(),
                    r.line,
                )
            })
            .collect();

        assert_eq!(
            summary,
            vec![
                (HttpMethod::Get, "/health", 12),
                (HttpMethod::Get, "/v1/customers/:id", 16),
                (HttpMethod::Post, "/v1/customers/:id", 16),
                (HttpMethod::Get, "/v1/posts/:slug", 20),
                (HttpMethod::Delete, "/v1/items/:id", 24),
                (HttpMethod::Put, "/settings", 28),
                (HttpMethod::Patch, "/settings", 32),
            ]
        );
        assert_eq!(
            routes[1].snippet,
            r#"@bp.route("/customers/<int:id>", methods=["GET", "POST"])"#
        );
    }

    #[test]
    fn preserves_line_numbers_across_multiline_decorators_and_unicode_comments() {
        let py = r#"# Café route file — ignored comment with UTF-8
from fastapi import FastAPI

app = FastAPI()

@app.api_route(
    "/cafes/{cafe_id:int}",
    methods=["GET", "DELETE"],
)
def cafe_handler(cafe_id: int):
    return {}
"#;

        let routes = extract_fastapi_routes("cafe-service", "main.py", py, None);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].line, 6);
        assert_eq!(routes[0].snippet, "@app.api_route(");
        assert_eq!(routes[0].endpoint.method, HttpMethod::Get);
        assert_eq!(routes[0].endpoint.normalized_path, "/cafes/:cafe_id");
        assert_eq!(routes[1].endpoint.method, HttpMethod::Delete);
        assert_eq!(routes[1].line, 6);
    }

    // Comprueba las reglas de prefijos en routers anidados y en fábricas Flask.
    #[test]
    fn extracts_nested_fastapi_routers_and_flask_app_factory_routes() {
        let fastapi = r#"from fastapi import APIRouter, FastAPI

app = FastAPI()
v1 = APIRouter(prefix="/v1")
customers = APIRouter(prefix="/customers")

@customers.get("/{id:int}")
def get_customer(id: int):
    return {}

v1.include_router(customers, prefix="/internal")
app.include_router(v1, prefix="/api")
"#;
        let fastapi_routes = extract_fastapi_routes("customer-service", "app.py", fastapi, None);
        assert_eq!(fastapi_routes.len(), 1);
        assert_eq!(
            fastapi_routes[0].endpoint.normalized_path,
            "/api/v1/internal/customers/:id"
        );

        let flask = r#"from flask import Blueprint, Flask

def create_app():
    app = Flask(__name__)
    bp = Blueprint("codes", __name__, url_prefix="/codes")

    @bp.route("/<string(length=2):code>", methods=("GET", "PATCH"))
    def handle_code(code):
        return {}

    app.register_blueprint(bp, url_prefix="/api")
    return app
"#;
        let flask_routes = extract_flask_routes("code-service", "factory.py", flask, None);
        let summary: Vec<(HttpMethod, &str, usize)> = flask_routes
            .iter()
            .map(|r| {
                (
                    r.endpoint.method,
                    r.endpoint.normalized_path.as_str(),
                    r.line,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (HttpMethod::Get, "/api/:code", 7),
                (HttpMethod::Patch, "/api/:code", 7),
            ]
        );
    }

    // Verifica el prefijo por defecto y las sustituciones en montajes del mismo blueprint.
    #[test]
    fn resolves_default_empty_and_nested_blueprint_prefixes() {
        let flask = r#"from flask import Blueprint, Flask
app = Flask(__name__)
outer = Blueprint("outer", __name__, url_prefix="/outer-default")
child = Blueprint("child", __name__, url_prefix="/child-default")

@child.get("/x")
def handler():
    return {}

outer.register_blueprint(child, url_prefix="/child-override")
app.register_blueprint(outer, url_prefix="/v2")
app.register_blueprint(child)
app.register_blueprint(child, name="child_empty", url_prefix="")
"#;
        let routes = extract_flask_routes("service", "app.py", flask, Some("/proxy"));
        let paths: Vec<_> = routes
            .iter()
            .map(|route| route.endpoint.normalized_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "/proxy/v2/child-override/x",
                "/proxy/child-default/x",
                "/proxy/x"
            ]
        );
    }
}
