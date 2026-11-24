use crate::model::{EndpointId, HttpMethod};
use crate::routes::{ExtractedRoute, normalize_route_path};

/// Extracts Laravel HTTP routes from a PHP route file (e.g. `routes/api.php`, `routes/web.php`).
///
/// Supports:
/// - `Route::get/post/put/patch/delete('path', ...)`
/// - `Route::match(['get', 'post'], 'path', ...)`
/// - `Route::any('path', ...)`
/// - Nested `Route::prefix('...')->group(function () { ... })`
/// - Nested `Route::group(['prefix' => '...'], function () { ... })`
/// - Chained `->prefix('...')` inside `Route::controller(...)->prefix('...')->group(...)`
/// - Comment skipping (`//`, `#`, `/* ... */`) while preserving exact 1-based line numbers
pub fn extract_laravel_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Vec<ExtractedRoute> {
    let cleaned = strip_php_comments_preserving_lines(source);
    let raw_lines: Vec<&str> = source.lines().collect();

    let mut routes = Vec::new();
    let mut brace_depth: usize = 0;
    let mut prefix_stack: Vec<(usize, String)> = Vec::new();
    let mut pending_prefix: Option<String> = None;

    let bytes = cleaned.as_bytes();
    let mut i = 0;
    let mut current_line = 1usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                current_line += 1;
                i += 1;
            }
            b'{' => {
                brace_depth += 1;
                if let Some(prefix) = pending_prefix.take() {
                    prefix_stack.push((brace_depth, prefix));
                }
                i += 1;
            }
            b'}' => {
                while prefix_stack
                    .last()
                    .is_some_and(|(depth, _)| *depth >= brace_depth)
                {
                    prefix_stack.pop();
                }
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
            }
            b'\'' | b'"' => {
                let (next_i, newlines) = skip_php_string(bytes, i);
                current_line += newlines;
                i = next_i;
            }
            _ => {
                // Check for `Route::`
                if starts_with_bytes(&bytes[i..], b"Route::") {
                    let after_route = i + b"Route::".len();
                    let (ident, after_ident) = read_ident(bytes, after_route);
                    let ident_lower = ident.to_ascii_lowercase();

                    if let Some(method) = HttpMethod::parse(&ident_lower) {
                        if let Some((path, next_pos, nl)) =
                            parse_first_string_arg(bytes, after_ident)
                        {
                            push_route(
                                &mut routes,
                                service,
                                file_path,
                                current_line,
                                &raw_lines,
                                mount_prefix,
                                &prefix_stack,
                                method,
                                &path,
                            );
                            current_line += nl;
                            i = next_pos;
                            continue;
                        }
                    } else if ident_lower == "match" {
                        if let Some((methods, path, next_pos, nl)) =
                            parse_route_match(bytes, after_ident)
                        {
                            for method in methods {
                                push_route(
                                    &mut routes,
                                    service,
                                    file_path,
                                    current_line,
                                    &raw_lines,
                                    mount_prefix,
                                    &prefix_stack,
                                    method,
                                    &path,
                                );
                            }
                            current_line += nl;
                            i = next_pos;
                            continue;
                        }
                    } else if ident_lower == "any" {
                        if let Some((path, next_pos, nl)) =
                            parse_first_string_arg(bytes, after_ident)
                        {
                            for method in [
                                HttpMethod::Get,
                                HttpMethod::Post,
                                HttpMethod::Put,
                                HttpMethod::Patch,
                                HttpMethod::Delete,
                            ] {
                                push_route(
                                    &mut routes,
                                    service,
                                    file_path,
                                    current_line,
                                    &raw_lines,
                                    mount_prefix,
                                    &prefix_stack,
                                    method,
                                    &path,
                                );
                            }
                            current_line += nl;
                            i = next_pos;
                            continue;
                        }
                    } else if ident_lower == "prefix" {
                        if let Some((prefix, next_pos, nl)) =
                            parse_first_string_arg(bytes, after_ident)
                        {
                            pending_prefix = Some(prefix);
                            current_line += nl;
                            i = next_pos;
                            continue;
                        }
                    } else if ident_lower == "group"
                        && let Some((prefix, next_pos, nl)) =
                            parse_group_array_prefix(bytes, after_ident)
                    {
                        pending_prefix = Some(prefix);
                        current_line += nl;
                        i = next_pos;
                        continue;
                    }
                    i = after_ident;
                    continue;
                }

                // Check for chained `->prefix('...')`
                if starts_with_bytes(&bytes[i..], b"->prefix") {
                    let after_prefix = i + b"->prefix".len();
                    if let Some((prefix, next_pos, nl)) =
                        parse_first_string_arg(bytes, after_prefix)
                    {
                        pending_prefix = Some(prefix);
                        current_line += nl;
                        i = next_pos;
                        continue;
                    }
                }

                i += 1;
            }
        }
    }

    routes
}

#[allow(clippy::too_many_arguments)]
fn push_route(
    out: &mut Vec<ExtractedRoute>,
    service: &str,
    file_path: &str,
    line: usize,
    raw_lines: &[&str],
    mount_prefix: Option<&str>,
    prefix_stack: &[(usize, String)],
    method: HttpMethod,
    raw_path: &str,
) {
    let mut prefixes: Vec<&str> = Vec::new();
    if let Some(mp) = mount_prefix {
        prefixes.push(mp);
    }
    for (_, p) in prefix_stack {
        prefixes.push(p.as_str());
    }

    let normalized_path = normalize_route_path(&prefixes, raw_path);
    let snippet = raw_lines
        .get(line.saturating_sub(1))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    out.push(ExtractedRoute {
        endpoint: EndpointId {
            service: service.to_string(),
            method,
            normalized_path,
        },
        file: file_path.to_string(),
        line,
        snippet,
    });
}

fn strip_php_comments_preserving_lines(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                out.push(quote as char);
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i];
                    out.push(ch as char);
                    if ch == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                        out.push(bytes[i] as char);
                        i += 1;
                        continue;
                    }
                    i += 1;
                    if ch == quote {
                        break;
                    }
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    if bytes[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            ch => {
                out.push(ch as char);
                i += 1;
            }
        }
    }

    out
}

fn starts_with_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && &haystack[..needle.len()] == needle
}

fn read_ident(bytes: &[u8], start: usize) -> (String, usize) {
    let mut i = start;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    let text = String::from_utf8_lossy(&bytes[start..i]).into_owned();
    (text, i)
}

fn skip_ws(bytes: &[u8], mut i: usize) -> (usize, usize) {
    let mut nl = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        if bytes[i] == b'\n' {
            nl += 1;
        }
        i += 1;
    }
    (i, nl)
}

fn skip_php_string(bytes: &[u8], start: usize) -> (usize, usize) {
    let quote = bytes[start];
    let mut i = start + 1;
    let mut nl = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\n' {
                nl += 1;
            }
            i += 2;
            continue;
        }
        if bytes[i] == b'\n' {
            nl += 1;
        }
        let ch = bytes[i];
        i += 1;
        if ch == quote {
            break;
        }
    }
    (i, nl)
}

fn read_php_string(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let quote = *bytes.get(start)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut i = start + 1;
    let mut nl = 0;
    let mut out = String::new();
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\n' {
                nl += 1;
            }
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return Some((out, i + 1, nl));
        }
        if bytes[i] == b'\n' {
            nl += 1;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    None
}

fn parse_first_string_arg(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let (i, mut nl) = skip_ws(bytes, start);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    let (i, nl2) = skip_ws(bytes, i + 1);
    nl += nl2;
    let (val, next, nl3) = read_php_string(bytes, i)?;
    Some((val, next, nl + nl3))
}

fn parse_route_match(
    bytes: &[u8],
    start: usize,
) -> Option<(Vec<HttpMethod>, String, usize, usize)> {
    let (i, mut nl) = skip_ws(bytes, start);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    let (mut i, nl2) = skip_ws(bytes, i + 1);
    nl += nl2;
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;

    let mut methods = Vec::new();
    while i < bytes.len() && bytes[i] != b']' {
        let (next_i, nl_w) = skip_ws(bytes, i);
        nl += nl_w;
        i = next_i;
        if let Some((m_str, after_s, nl_s)) = read_php_string(bytes, i) {
            nl += nl_s;
            i = after_s;
            if let Some(m) = HttpMethod::parse(&m_str) {
                methods.push(m);
            }
        } else if bytes.get(i) == Some(&b',') {
            i += 1;
        } else {
            break;
        }
    }

    if bytes.get(i) != Some(&b']') {
        return None;
    }
    i += 1;
    let (i, nl3) = skip_ws(bytes, i);
    nl += nl3;
    if bytes.get(i) != Some(&b',') {
        return None;
    }
    let (i, nl4) = skip_ws(bytes, i + 1);
    nl += nl4;
    let (path, next_pos, nl5) = read_php_string(bytes, i)?;
    Some((methods, path, next_pos, nl + nl5))
}

fn parse_group_array_prefix(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let (i, mut nl) = skip_ws(bytes, start);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    let (mut i, nl2) = skip_ws(bytes, i + 1);
    nl += nl2;
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut bracket_depth = 1usize;

    while i < bytes.len() && bracket_depth > 0 {
        let (next_i, nl_w) = skip_ws(bytes, i);
        nl += nl_w;
        i = next_i;
        if i >= bytes.len() {
            break;
        }

        if bytes[i] == b'[' {
            bracket_depth += 1;
            i += 1;
            continue;
        }
        if bytes[i] == b']' {
            bracket_depth -= 1;
            i += 1;
            continue;
        }

        if bracket_depth == 1
            && let Some((key, after_k, nl_k)) = read_php_string(bytes, i)
        {
            nl += nl_k;
            i = after_k;
            let (after_ws, nl_w2) = skip_ws(bytes, i);
            nl += nl_w2;
            i = after_ws;

            if starts_with_bytes(&bytes[i..], b"=>") {
                let (val_pos, nl_w3) = skip_ws(bytes, i + 2);
                nl += nl_w3;
                if key == "prefix"
                    && let Some((val, after_v, nl_v)) = read_php_string(bytes, val_pos)
                {
                    return Some((val, after_v, nl + nl_v));
                }
                i = val_pos;
            }
        } else if bytes[i] == b'\'' || bytes[i] == b'"' {
            let (next_i, nl_s) = skip_php_string(bytes, i);
            nl += nl_s;
            i = next_i;
        } else {
            if bytes[i] == b'\n' {
                nl += 1;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_laravel_routes_with_prefixes_groups_and_comments() {
        let php = r#"<?php
use Illuminate\Support\Facades\Route;

// Route::get('/commented-out', 'Foo@bar');
/*
  Route::post('/block-commented', 'Foo@baz');
*/

Route::controller(AuthController::class)->group(function () {
    Route::post('/login', 'login');
    Route::get('reserva-calculo/{num_reserva}/{agencia_salida}', 'reservaCalculada');
});

Route::prefix('corp')->middleware(['auth:sanctum'])->group(function () {
    Route::post('/listBooking', 'listBooking');
    Route::post('abortLocalBooking', 'abortLocalBooking');
});

Route::group(['middleware' => ['auth'], 'prefix' => 'web'], function () {
    Route::post('/getBooking', 'getBooking');
});

Route::match(['get', 'post'], '/health', 'HealthController@check');
"#;

        let routes = extract_laravel_routes("api-service", "routes/api.php", php, Some("/api"));
        let summary: Vec<(HttpMethod, &str)> = routes
            .iter()
            .map(|r| (r.endpoint.method, r.endpoint.normalized_path.as_str()))
            .collect();

        assert_eq!(
            summary,
            vec![
                (HttpMethod::Post, "/api/login"),
                (
                    HttpMethod::Get,
                    "/api/reserva-calculo/:num_reserva/:agencia_salida"
                ),
                (HttpMethod::Post, "/api/corp/listBooking"),
                (HttpMethod::Post, "/api/corp/abortLocalBooking"),
                (HttpMethod::Post, "/api/web/getBooking"),
                (HttpMethod::Get, "/api/health"),
                (HttpMethod::Post, "/api/health"),
            ]
        );
    }
}
