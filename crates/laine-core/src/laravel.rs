use crate::model::{EndpointId, HttpMethod};
use crate::routes::{ExtractedRoute, normalize_route_path};

/// Extrae rutas estáticas de Laravel y mantiene la API de los catálogos de repositorios.
pub fn extract_laravel_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> Vec<ExtractedRoute> {
    analyze_laravel_routes(service, file_path, source, mount_prefix).0
}

/// Devuelve las rutas y las líneas de declaraciones que quedaron sin resolver.
pub fn analyze_laravel_routes(
    service: &str,
    file_path: &str,
    source: &str,
    mount_prefix: Option<&str>,
) -> (Vec<ExtractedRoute>, Vec<usize>) {
    let cleaned = strip_php_comments_preserving_lines(source);
    let raw_lines: Vec<&str> = source.lines().collect();
    let bytes = &cleaned;
    let mut routes = Vec::new();
    let mut unresolved = Vec::new();
    let mut brace_depth = 0usize;
    let mut prefix_stack = Vec::new();
    let mut pending_group = None;
    let mut i = 0;
    let mut line = 1;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => line += 1,
            b'{' => {
                brace_depth += 1;
                if let Some(prefix) = pending_group.take() {
                    prefix_stack.push((brace_depth, prefix));
                }
            }
            b'}' => {
                if prefix_stack
                    .last()
                    .is_some_and(|(depth, _)| *depth == brace_depth)
                {
                    prefix_stack.pop();
                }
                brace_depth = brace_depth.saturating_sub(1);
            }
            b';' => pending_group = None,
            b'\'' | b'"' => {
                let (next, newlines) = skip_php_string(bytes, i);
                i = next;
                line += newlines;
                continue;
            }
            b'R' if bytes[i..].starts_with(b"Route::")
                && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')) =>
            {
                let start = i + b"Route::".len();
                if let Some(declaration) = read_route_declaration(bytes, start) {
                    let operation = declaration.method.as_str();
                    let args = declaration.args;
                    if operation == "group" {
                        pending_group = group_prefix(bytes, declaration);
                        if pending_group.is_none() {
                            unresolved.push(line);
                        }
                        line += count_newlines(&bytes[i..args]);
                        i = args;
                        continue;
                    }
                    let outer_len = prefix_stack.len();
                    if let Some(prefix) = declaration.prefix.clone() {
                        prefix_stack.push((brace_depth, prefix));
                    }
                    match declaration_endpoints(bytes, declaration) {
                        Some(endpoints) => {
                            for (method, path) in endpoints {
                                push_route(
                                    &mut routes,
                                    service,
                                    file_path,
                                    line,
                                    &raw_lines,
                                    mount_prefix,
                                    &prefix_stack,
                                    method,
                                    &path,
                                );
                            }
                        }
                        None => unresolved.push(line),
                    }
                    prefix_stack.truncate(outer_len);
                    line += count_newlines(&bytes[i..args]);
                    i = args;
                    continue;
                }
                unresolved.push(line);
                i = start;
                continue;
            }
            _ => {}
        }
        i += 1;
    }

    (routes, unresolved)
}

struct RouteDeclaration {
    method: String,
    args: usize,
    prefix: Option<String>,
}

// Sigue una cadena de atributos estáticos hasta la llamada que declara la ruta o el grupo.
fn read_route_declaration(bytes: &[u8], mut start: usize) -> Option<RouteDeclaration> {
    let mut prefix = None;
    loop {
        let (name, args) = read_ident(bytes, skip_ws(bytes, start).0);
        let method = name.to_ascii_lowercase();
        if !matches!(
            method.as_str(),
            "prefix"
                | "middleware"
                | "controller"
                | "name"
                | "namespace"
                | "domain"
                | "where"
                | "withoutmiddleware"
                | "scopebindings"
                | "withoutscopedbindings"
                | "as"
        ) {
            return Some(RouteDeclaration {
                method,
                args,
                prefix,
            });
        }
        if method == "prefix" {
            prefix = Some(parse_first_string_arg(bytes, args)?.0);
        }
        let end = skip_php_arguments(bytes, args)?.0;
        let next = skip_ws(bytes, end).0;
        if !bytes[next..].starts_with(b"->") {
            return None;
        }
        start = next + 2;
    }
}

// Reconoce un cierre de grupo y recoge el prefijo declarado mediante una cadena o arreglo.
fn group_prefix(bytes: &[u8], declaration: RouteDeclaration) -> Option<String> {
    let (start, _) = skip_ws(bytes, declaration.args);
    if bytes.get(start) != Some(&b'(') {
        return None;
    }
    let opening = skip_ws(bytes, start + 1).0;
    let (position, array_prefix) = if bytes.get(opening) == Some(&b'[') {
        let (end, _) = skip_php_arguments(bytes, opening)?;
        let comma = skip_ws(bytes, end).0;
        if bytes.get(comma) != Some(&b',') {
            return None;
        }
        let prefix = parse_group_array_prefix(bytes, declaration.args)?.0;
        (skip_ws(bytes, comma + 1).0, prefix)
    } else {
        (opening, String::new())
    };
    let (closure, _) = read_ident(bytes, position);
    if !matches!(closure.as_str(), "function" | "static") {
        return None;
    }
    Some(normalize_route_path(
        &[declaration.prefix.as_deref().unwrap_or_default()],
        &array_prefix,
    ))
}

// Resuelve verbos literales y expande los métodos de los recursos compatibles.
fn declaration_endpoints(
    bytes: &[u8],
    declaration: RouteDeclaration,
) -> Option<Vec<(HttpMethod, String)>> {
    let args = declaration.args;
    match declaration.method.as_str() {
        "match" => {
            let (methods, path, _, _) = parse_route_match(bytes, args)?;
            Some(
                methods
                    .into_iter()
                    .map(|method| (method, path.clone()))
                    .collect(),
            )
        }
        "any" => {
            let (path, _, _) = parse_first_string_arg(bytes, args)?;
            Some(
                [
                    HttpMethod::Get,
                    HttpMethod::Post,
                    HttpMethod::Put,
                    HttpMethod::Patch,
                    HttpMethod::Delete,
                ]
                .into_iter()
                .map(|method| (method, path.clone()))
                .collect(),
            )
        }
        "pattern" | "patterns" | "model" | "bind" => Some(Vec::new()),
        "resource" | "apiresource" => {
            expand_resource(bytes, args, declaration.method == "apiresource")
        }
        method => {
            let method = HttpMethod::parse(method)?;
            let (path, _, _) = parse_first_string_arg(bytes, args)?;
            Some(vec![(method, path)])
        }
    }
}

// Recorre argumentos balanceados y registra separadores de la lista exterior.
fn skip_php_arguments(bytes: &[u8], start: usize) -> Option<(usize, Vec<usize>)> {
    let mut i = skip_ws(bytes, start).0;
    let closing = match bytes.get(i)? {
        b'(' => b')',
        b'[' => b']',
        _ => return None,
    };
    let mut stack = vec![closing];
    let mut commas = Vec::new();
    i += 1;
    while let Some(&token) = bytes.get(i) {
        match token {
            b'\'' | b'"' => {
                i = skip_php_string(bytes, i).0;
                continue;
            }
            b'(' => stack.push(b')'),
            b'[' => stack.push(b']'),
            b'{' => stack.push(b'}'),
            b')' | b']' | b'}' => {
                if stack.pop() != Some(token) {
                    return None;
                }
                if stack.is_empty() {
                    return Some((i + 1, commas));
                }
            }
            b',' if stack.len() == 1 => commas.push(i),
            _ => {}
        }
        i += 1;
    }
    None
}

// Conserva el catálogo de rutas de un recurso simple y reconoce filtros only/except literales.
fn expand_resource(bytes: &[u8], args: usize, api: bool) -> Option<Vec<(HttpMethod, String)>> {
    let (path, _, _) = parse_first_string_arg(bytes, args)?;
    if path.trim_matches('/').is_empty() || path.contains(['.', '{', '}']) {
        return None;
    }
    let (mut end, commas) = skip_php_arguments(bytes, args)?;
    let controller_offset = *commas.first()?;
    if skip_ws(bytes, controller_offset + 1).0 >= end - 1
        || commas
            .iter()
            .skip(1)
            .any(|pos| skip_ws(bytes, pos + 1).0 != end - 1)
    {
        return None;
    }
    let mut only = None;
    let mut except = None;
    loop {
        let next = skip_ws(bytes, end).0;
        if !bytes[next..].starts_with(b"->") {
            break;
        }
        let (modifier, args) = read_ident(bytes, skip_ws(bytes, next + 2).0);
        let (after, _) = skip_php_arguments(bytes, args)?;
        match modifier.to_ascii_lowercase().as_str() {
            "only" => only = Some(read_resource_actions(bytes, args)?),
            "except" => except = Some(read_resource_actions(bytes, args)?),
            "names" | "name" | "middleware" | "withoutmiddleware" | "parameters" | "parameter"
            | "where" | "scoped" | "missing" | "withtrashed" | "register" => {}
            _ => return None,
        }
        end = after;
    }
    let path = path.trim_matches('/');
    let mut routes = Vec::new();
    for (action, methods, suffix) in [
        ("index", &[HttpMethod::Get][..], ""),
        ("create", &[HttpMethod::Get][..], "/create"),
        ("store", &[HttpMethod::Post][..], ""),
        ("show", &[HttpMethod::Get][..], "/{resource}"),
        ("edit", &[HttpMethod::Get][..], "/{resource}/edit"),
        (
            "update",
            &[HttpMethod::Put, HttpMethod::Patch][..],
            "/{resource}",
        ),
        ("destroy", &[HttpMethod::Delete][..], "/{resource}"),
    ] {
        let enabled = only
            .as_ref()
            .map_or(!(api && matches!(action, "create" | "edit")), |items| {
                items.iter().any(|item| item == action)
            })
            && except
                .as_ref()
                .is_none_or(|items| !items.iter().any(|item| item == action));
        if enabled {
            routes.extend(
                methods
                    .iter()
                    .map(|method| (*method, format!("{path}{suffix}"))),
            );
        }
    }
    Some(routes)
}

// Lee nombres de acciones literales, separados por comas o dentro de un arreglo.
fn read_resource_actions(bytes: &[u8], args: usize) -> Option<Vec<String>> {
    let opening = skip_ws(bytes, args).0;
    let first = skip_ws(bytes, opening + 1).0;
    let in_array = bytes.get(first) == Some(&b'[');
    let list = if in_array { first } else { opening };
    let (end, commas) = skip_php_arguments(bytes, list)?;
    if in_array && bytes.get(skip_ws(bytes, end).0) != Some(&b')') {
        return None;
    }
    let mut actions = Vec::new();
    let mut start = list + 1;
    for boundary in commas.into_iter().chain([end - 1]) {
        start = skip_ws(bytes, start).0;
        if start == end - 1 {
            break;
        }
        let (action, next, _) = read_php_string(bytes, start)?;
        if skip_ws(bytes, next).0 != boundary {
            return None;
        }
        actions.push(action);
        start = boundary + 1;
    }
    Some(actions)
}

// Cuenta las líneas consumidas por un tramo de cadena antes de retomar el escaneo.
fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
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

// Sustituye comentarios por espacios para mantener las posiciones y el UTF-8 original.
fn strip_php_comments_preserving_lines(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\'' | b'"') {
            i = skip_php_string(bytes, i).0;
            continue;
        }
        let end = if bytes[i..].starts_with(b"/*") {
            bytes[i + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |offset| i + 2 + offset + 2)
        } else if bytes[i..].starts_with(b"//")
            || (bytes[i] == b'#' && bytes.get(i + 1) != Some(&b'['))
        {
            bytes[i..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(bytes.len(), |offset| i + offset)
        } else {
            i += 1;
            continue;
        };
        for byte in &mut out[i..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        i = end;
    }
    out
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

// Decodifica cadenas PHP literales sin convertir bytes UTF-8 de forma independiente.
fn read_php_string(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let mut i = start + 1;
    let mut out = Vec::new();
    while let Some(&byte) = bytes.get(i) {
        if byte == quote {
            return Some((
                String::from_utf8(out).ok()?,
                i + 1,
                count_newlines(&bytes[start..i]),
            ));
        }
        if byte == b'\\' {
            let next = *bytes.get(i + 1)?;
            match (quote, next) {
                (_, b'\\') | (b'\'', b'\'') | (b'"', b'"' | b'$') => out.push(next),
                (b'"', b'n') => out.push(b'\n'),
                (b'"', b'r') => out.push(b'\r'),
                (b'"', b't') => out.push(b'\t'),
                (b'"', b'v') => out.push(0x0b),
                (b'"', b'e') => out.push(0x1b),
                (b'"', b'f') => out.push(0x0c),
                (b'"', b'0'..=b'7' | b'x' | b'u') => return None,
                _ => {
                    out.push(byte);
                    out.push(next);
                }
            }
            i += 2;
            continue;
        }
        if quote == b'"'
            && byte == b'$'
            && bytes.get(i + 1).is_some_and(|&next| {
                next == b'{' || next == b'_' || next.is_ascii_alphabetic() || next >= 0x80
            })
        {
            return None;
        }
        out.push(byte);
        i += 1;
    }
    None
}

// Exige que el primer argumento sea una cadena completa y estática.
fn parse_first_string_arg(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let (i, mut nl) = skip_ws(bytes, start);
    if bytes.get(i) != Some(&b'(') {
        return None;
    }
    let (i, nl2) = skip_ws(bytes, i + 1);
    nl += nl2;
    let (val, next, nl3) = read_php_string(bytes, i)?;
    if !matches!(bytes.get(skip_ws(bytes, next).0), Some(b',' | b')')) {
        return None;
    }
    Some((val, next, nl + nl3))
}

// Lee un arreglo de métodos soportados seguido por una ruta literal.
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
            methods.push(HttpMethod::parse(&m_str)?);
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
    if methods.is_empty() || !matches!(bytes.get(skip_ws(bytes, next_pos).0), Some(b',' | b')')) {
        return None;
    }
    Some((methods, path, next_pos, nl + nl5))
}

// Lee el prefijo literal de un arreglo de atributos, o devuelve vacío si no lo contiene.
fn parse_group_array_prefix(bytes: &[u8], start: usize) -> Option<(String, usize, usize)> {
    let opening = skip_ws(bytes, skip_ws(bytes, start).0 + 1).0;
    let (end, commas) = skip_php_arguments(bytes, opening)?;
    let mut index = opening + 1;
    for boundary in commas.into_iter().chain([end - 1]) {
        index = skip_ws(bytes, index).0;
        if index == end - 1 {
            break;
        }
        let (key, next, _) = read_php_string(bytes, index)?;
        let arrow = skip_ws(bytes, next).0;
        if !bytes[arrow..].starts_with(b"=>") {
            return None;
        }
        if key == "prefix" {
            let (prefix, pos, _) = read_php_string(bytes, skip_ws(bytes, arrow + 2).0)?;
            if skip_ws(bytes, pos).0 != boundary {
                return None;
            }
            return Some((prefix, end, count_newlines(&bytes[start..end])));
        }
        index = boundary + 1;
    }
    Some((String::new(), end, count_newlines(&bytes[start..end])))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verifica cadenas de registro, recursos y literales UTF-8 sin perder la evidencia.
    #[test]
    fn expands_laravel_resources_and_preserves_unicode_spans() {
        let php = r#"<?php
Route::middleware(['auth'])
    ->prefix('región')
    ->group(['prefix' => 'ventas'], function () {
        Route::middleware('auth')
            ->get('/año', Handler::class);
        Route::apiResource('orders', OrderController::class)->except('destroy');
    });
Route::resource('pages', PageController::class)->except('edit')->only(['index', 'create', 'edit']);
Route::get('/literal' . $suffix, Handler::class);
Route::get("/$dynamic", Handler::class);
Route::get('/último', Handler::class);
"#;
        let (routes, unresolved) =
            analyze_laravel_routes("api", "routes/api.php", php, Some("/api"));
        let actual: Vec<_> = routes
            .iter()
            .map(|route| {
                (
                    route.endpoint.method,
                    route.endpoint.normalized_path.as_str(),
                    route.line,
                )
            })
            .collect();
        assert_eq!(
            actual,
            [
                (HttpMethod::Get, "/api/región/ventas/año", 5),
                (HttpMethod::Get, "/api/región/ventas/orders", 7),
                (HttpMethod::Post, "/api/región/ventas/orders", 7),
                (HttpMethod::Get, "/api/región/ventas/orders/:resource", 7),
                (HttpMethod::Put, "/api/región/ventas/orders/:resource", 7),
                (HttpMethod::Patch, "/api/región/ventas/orders/:resource", 7),
                (HttpMethod::Get, "/api/pages", 9),
                (HttpMethod::Get, "/api/pages/create", 9),
                (HttpMethod::Get, "/api/último", 12),
            ]
        );
        assert_eq!(routes[0].snippet, "Route::middleware('auth')");
        assert_eq!(unresolved, [10, 11]);
    }

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
