use oxc_span::SourceType;

/// An extracted `<script>` block from a Vue Single File Component (`.vue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VueScriptBlock {
    /// Script source with leading `\n` padding so 1-based line numbers match the original `.vue` file.
    pub code: String,
    pub lang: VueScriptLang,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VueScriptLang {
    #[default]
    Js,
    Jsx,
    Ts,
    Tsx,
}

impl VueScriptLang {
    // Selecciona el mismo modo de parser para todos los extractores.
    pub(crate) fn source_type(self) -> SourceType {
        match self {
            Self::Js => SourceType::mjs(),
            Self::Jsx => SourceType::jsx().with_module(true),
            Self::Ts => SourceType::ts().with_module(true),
            Self::Tsx => SourceType::tsx().with_module(true),
        }
    }
}

/// A JavaScript/TypeScript expression extracted from a Vue Single File Component `<template>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VueTemplateExpr {
    pub expr: String,
    pub line: usize,
    pub snippet: String,
}

/// Extracts every `<script ...>...</script>` block from a `.vue` file.
///
/// Exact 1-based line numbers from the original `.vue` file are preserved by prepending
/// the exact number of `\n` characters that appeared before each script body.
pub fn extract_vue_scripts(source: &str) -> Vec<VueScriptBlock> {
    extract_vue_scripts_with_setup(source)
        .into_iter()
        .map(|(block, _)| block)
        .collect()
}

/// Extracts every `<script ...>...</script>` block along with a flag indicating
/// whether the opening `<script>` tag includes the `setup` attribute.
// Conserva el lenguaje y la posición original de cada bloque de script.
pub(crate) fn extract_vue_scripts_with_setup(source: &str) -> Vec<(VueScriptBlock, bool)> {
    let lower = source.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut blocks = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(rel_lt) = lower[cursor..].find('<') else {
            break;
        };
        let pos = cursor + rel_lt;

        // Skip HTML comments so commented-out `<script>` blocks are ignored.
        if lower[pos..].starts_with("<!--") {
            match lower[pos + 4..].find("-->") {
                Some(rel_end) => {
                    cursor = pos + 4 + rel_end + 3;
                    continue;
                }
                None => break,
            }
        }

        if starts_with_tag(&lower, pos, "<script") {
            let after_name = pos + "<script".len();
            let Some(tag_end) = find_opening_tag_end(bytes, after_name) else {
                break;
            };
            let attrs = &lower[after_name..tag_end];
            if attrs.trim_end().ends_with('/') {
                // Self-closing `<script src="..." />` has no inline body.
                cursor = tag_end + 1;
                continue;
            }

            let (lang, is_setup) = parse_script_tag_attrs(attrs);
            let body_start = tag_end + 1;
            let Some((body_end, after_close)) = find_closing_tag(&lower, "</script", body_start)
            else {
                break;
            };

            let leading_newlines = source[..body_start].bytes().filter(|&b| b == b'\n').count();
            let body = &source[body_start..body_end];

            let mut code = String::with_capacity(leading_newlines + body.len());
            for _ in 0..leading_newlines {
                code.push('\n');
            }
            code.push_str(body);

            blocks.push((VueScriptBlock { code, lang }, is_setup));
            cursor = after_close;
            continue;
        }

        cursor = pos + 1;
    }

    blocks
}

/// Extracts JS/TS expressions from `<template>...</template>` blocks in a Vue SFC.
///
/// Tracks nested `<template>` tag depth (e.g. `<template v-if="...">`), ignores HTML comments
/// (`<!-- ... -->`), skips `<script>` and `<style>` blocks, and preserves exact 1-based line
/// numbers and trimmed line snippets from `source`.
pub fn extract_vue_template_expressions(source: &str) -> Vec<VueTemplateExpr> {
    let lower = source.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let line_index = LineIndex::new(source);
    let mut exprs = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let Some(rel_lt) = lower[cursor..].find('<') else {
            break;
        };
        let pos = cursor + rel_lt;

        // Skip top-level HTML comments.
        if lower[pos..].starts_with("<!--") {
            match lower[pos + 4..].find("-->") {
                Some(rel_end) => {
                    cursor = pos + 4 + rel_end + 3;
                    continue;
                }
                None => break,
            }
        }

        // Skip `<script>` and `<style>` blocks so strings inside JS/CSS cannot fake `<template>`.
        if starts_with_tag(&lower, pos, "<script") {
            let after_name = pos + "<script".len();
            let Some(tag_end) = find_opening_tag_end(bytes, after_name) else {
                break;
            };
            if lower[after_name..tag_end].trim_end().ends_with('/') {
                cursor = tag_end + 1;
                continue;
            }
            match find_closing_tag(&lower, "</script", tag_end + 1) {
                Some((_, after_close)) => {
                    cursor = after_close;
                    continue;
                }
                None => break,
            }
        }

        if starts_with_tag(&lower, pos, "<style") {
            let after_name = pos + "<style".len();
            let Some(tag_end) = find_opening_tag_end(bytes, after_name) else {
                break;
            };
            if lower[after_name..tag_end].trim_end().ends_with('/') {
                cursor = tag_end + 1;
                continue;
            }
            match find_closing_tag(&lower, "</style", tag_end + 1) {
                Some((_, after_close)) => {
                    cursor = after_close;
                    continue;
                }
                None => break,
            }
        }

        if starts_with_tag(&lower, pos, "<template") {
            let after_name = pos + "<template".len();
            let Some(tag_end) = find_opening_tag_end(bytes, after_name) else {
                break;
            };
            if lower[after_name..tag_end].trim_end().ends_with('/') {
                cursor = tag_end + 1;
                continue;
            }

            cursor = scan_template_body(source, &lower, tag_end + 1, &line_index, &mut exprs);
            continue;
        }

        cursor = pos + 1;
    }

    exprs
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

    fn line_number(&self, byte_offset: usize) -> usize {
        match self.line_starts.binary_search(&byte_offset) {
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

fn scan_template_body(
    source: &str,
    lower: &str,
    start: usize,
    line_index: &LineIndex,
    exprs: &mut Vec<VueTemplateExpr>,
) -> usize {
    let bytes = lower.as_bytes();
    let mut cursor = start;
    let mut depth: usize = 1;

    while cursor < bytes.len() && depth > 0 {
        let b = bytes[cursor];

        // Only slice `source`/`lower` when `cursor` sits on an ASCII byte (`<` or `{`),
        // guaranteeing we are always on a valid UTF-8 character boundary.
        if b == b'<' {
            // 1. HTML comment inside `<template>`: ignore completely (including any tags or mustaches).
            if lower[cursor..].starts_with("<!--") {
                match lower[cursor + 4..].find("-->") {
                    Some(rel_end) => {
                        cursor = cursor + 4 + rel_end + 3;
                        continue;
                    }
                    None => return bytes.len(),
                }
            }

            // 2. Closing tag `</...>`.
            if cursor + 1 < bytes.len() && bytes[cursor + 1] == b'/' {
                let name_start = cursor + 2;
                let mut name_end = name_start;
                while name_end < bytes.len()
                    && (bytes[name_end].is_ascii_alphanumeric()
                        || matches!(bytes[name_end], b'-' | b'_' | b':'))
                {
                    name_end += 1;
                }
                let tag_name = &lower[name_start..name_end];
                let Some(tag_end) = find_opening_tag_end(bytes, name_end) else {
                    return bytes.len();
                };
                if tag_name == "template" {
                    depth -= 1;
                }
                cursor = tag_end + 1;
                continue;
            }

            // 3. Opening tag `<tagName ...>`.
            if cursor + 1 < bytes.len() && bytes[cursor + 1].is_ascii_alphabetic() {
                let name_start = cursor + 1;
                let mut name_end = name_start;
                while name_end < bytes.len()
                    && (bytes[name_end].is_ascii_alphanumeric()
                        || matches!(bytes[name_end], b'-' | b'_' | b':'))
                {
                    name_end += 1;
                }
                let tag_name = &lower[name_start..name_end];
                let Some(tag_end) = find_opening_tag_end(bytes, name_end) else {
                    return bytes.len();
                };
                let is_self_closing = lower[name_end..tag_end].trim_end().ends_with('/');
                let attrs_end = if is_self_closing {
                    let trimmed = lower[name_end..tag_end].trim_end();
                    name_end + trimmed.len() - 1
                } else {
                    tag_end
                };

                extract_tag_expressions(source, lower, name_end, attrs_end, line_index, exprs);

                if tag_name == "template" && !is_self_closing {
                    depth += 1;
                }
                cursor = tag_end + 1;
                continue;
            }

            cursor += 1;
            continue;
        }

        // 4. Mustache interpolation `{{ expr }}`.
        if b == b'{' && cursor + 1 < bytes.len() && bytes[cursor + 1] == b'{' {
            let expr_start = cursor + 2;
            if let Some(expr_end) = find_mustache_end(source, expr_start) {
                let raw_expr = &source[expr_start..expr_end];
                let trimmed = raw_expr.trim();
                if !trimmed.is_empty() {
                    let leading_ws = raw_expr.len() - raw_expr.trim_start().len();
                    let line = line_index.line_number(expr_start + leading_ws);
                    let snippet = line_index.line_snippet(source, line).to_string();
                    exprs.push(VueTemplateExpr {
                        expr: trimmed.to_string(),
                        line,
                        snippet,
                    });
                }
                cursor = expr_end + 2;
                continue;
            }
            return bytes.len();
        }

        cursor += 1;
    }

    cursor
}

enum VueAttrKind {
    Expr,
    VFor,
}

fn classify_vue_attr(attr_name_lower: &str) -> Option<VueAttrKind> {
    if attr_name_lower == "v-for" {
        return Some(VueAttrKind::VFor);
    }
    if (attr_name_lower.starts_with(':') && attr_name_lower.len() > 1)
        || attr_name_lower == "v-bind"
        || (attr_name_lower.starts_with("v-bind:") && attr_name_lower.len() > "v-bind:".len())
        || attr_name_lower == "v-if"
        || attr_name_lower == "v-else-if"
        || attr_name_lower == "v-show"
        || attr_name_lower == "v-text"
        || attr_name_lower == "v-html"
        || attr_name_lower == "v-model"
        || attr_name_lower.starts_with("v-model:")
        || attr_name_lower.starts_with("v-model.")
    {
        return Some(VueAttrKind::Expr);
    }
    None
}

fn extract_tag_expressions(
    source: &str,
    lower: &str,
    attrs_start: usize,
    attrs_end: usize,
    line_index: &LineIndex,
    exprs: &mut Vec<VueTemplateExpr>,
) {
    let bytes = lower.as_bytes();
    let mut i = attrs_start;

    while i < attrs_end {
        while i < attrs_end && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= attrs_end {
            break;
        }

        let name_start = i;
        while i < attrs_end
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let attr_name_lower = &lower[name_start..i];

        while i < attrs_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i < attrs_end && bytes[i] == b'=' {
            i += 1;
            while i < attrs_end && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= attrs_end {
                break;
            }

            let (val_start, val_end) = if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < attrs_end && bytes[i] != quote {
                    i += 1;
                }
                let end = i;
                if i < attrs_end {
                    i += 1;
                }
                (start, end)
            } else {
                let start = i;
                while i < attrs_end && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
                    i += 1;
                }
                (start, i)
            };

            let Some(kind) = classify_vue_attr(attr_name_lower) else {
                continue;
            };
            let raw_val = &source[val_start..val_end];

            match kind {
                VueAttrKind::Expr => {
                    let trimmed = raw_val.trim();
                    if !trimmed.is_empty() {
                        let leading_ws = raw_val.len() - raw_val.trim_start().len();
                        let line = line_index.line_number(val_start + leading_ws);
                        let snippet = line_index.line_snippet(source, line).to_string();
                        exprs.push(VueTemplateExpr {
                            expr: trimmed.to_string(),
                            line,
                            snippet,
                        });
                    }
                }
                VueAttrKind::VFor => {
                    if let Some((rhs_rel_offset, rhs_expr)) = extract_v_for_rhs(raw_val) {
                        let line = line_index.line_number(val_start + rhs_rel_offset);
                        let snippet = line_index.line_snippet(source, line).to_string();
                        exprs.push(VueTemplateExpr {
                            expr: rhs_expr.to_string(),
                            line,
                            snippet,
                        });
                    }
                }
            }
        }
    }
}

fn find_mustache_end(source: &str, expr_start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = expr_start;
    let mut quote: Option<u8> = None;
    let mut brace_depth: usize = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' | b'\'' | b'`' => {
                quote = Some(b);
                i += 1;
            }
            b'{' => {
                brace_depth += 1;
                i += 1;
            }
            b'}' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                    i += 1;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                    return Some(i);
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    None
}

fn extract_v_for_rhs(raw: &str) -> Option<(usize, &str)> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut paren_depth: usize = 0;
    let mut brace_depth: usize = 0;
    let mut bracket_depth: usize = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'i' | b'o'
                if paren_depth == 0
                    && brace_depth == 0
                    && bracket_depth == 0
                    && i > 0
                    && i + 2 < bytes.len() =>
            {
                let is_kw = (bytes[i] == b'i' && bytes[i + 1] == b'n')
                    || (bytes[i] == b'o' && bytes[i + 1] == b'f');
                let prev = bytes[i - 1];
                let next = bytes[i + 2];
                let valid_prev = prev.is_ascii_whitespace() || matches!(prev, b')' | b'}' | b']');
                let valid_next = next.is_ascii_whitespace() || matches!(next, b'(' | b'[');
                if is_kw && valid_prev && valid_next {
                    let rhs_raw = &raw[i + 2..];
                    let trimmed = rhs_raw.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    let leading_ws = rhs_raw.len() - rhs_raw.trim_start().len();
                    return Some((i + 2 + leading_ws, trimmed));
                }
            }
            _ => {}
        }
        i += 1;
    }

    None
}

fn starts_with_tag(lower: &str, pos: usize, tag: &str) -> bool {
    if !lower[pos..].starts_with(tag) {
        return false;
    }
    let after = pos + tag.len();
    let bytes = lower.as_bytes();
    after < bytes.len() && matches!(bytes[after], b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
}

fn find_opening_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn find_closing_tag(lower: &str, close_tag: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = lower.as_bytes();
    let mut search = start;

    while search < bytes.len() {
        let rel = lower[search..].find(close_tag)?;
        let close_pos = search + rel;
        let mut idx = close_pos + close_tag.len();

        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx < bytes.len() && bytes[idx] == b'>' {
            return Some((close_pos, idx + 1));
        }

        search = close_pos + close_tag.len();
    }

    None
}

// Distingue JS, JSX, TS y TSX en la etiqueta de apertura.
fn parse_script_tag_attrs(attrs: &str) -> (VueScriptLang, bool) {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    let mut lang = VueScriptLang::Js;
    let mut is_setup = false;

    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let name = &attrs[name_start..i];
        if name == "setup" {
            is_setup = true;
        }

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }

            let value = if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                i += 1;
                let val_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let val = &attrs[val_start..i];
                if i < bytes.len() {
                    i += 1;
                }
                val
            } else {
                let val_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
                    i += 1;
                }
                &attrs[val_start..i]
            };

            if name == "lang" {
                lang = match value.trim() {
                    "jsx" => VueScriptLang::Jsx,
                    "ts" => VueScriptLang::Ts,
                    "tsx" => VueScriptLang::Tsx,
                    _ => VueScriptLang::Js,
                };
            }
        }
    }

    (lang, is_setup)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use laine_core::config::ServiceConfig;
    use laine_core::model::HttpMethod;

    use crate::extractor::{HttpClientKind, extract_file};

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
    fn extracts_vue_script_setup_ts_and_preserves_exact_line_numbers() {
        let sfc = "<template>\n  <div>{{ label }}</div>\n</template>\n\n<script setup lang=\"ts\">\nconst label = 'hello';\nconst count: number = 42;\n</script>\n";
        let blocks = extract_vue_scripts(sfc);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lang, VueScriptLang::Ts);

        let lines: Vec<&str> = blocks[0].code.lines().collect();
        // Lines 1..=5 are empty (template + blank + opening `<script>` line), line 6 is `const label = 'hello';`.
        assert_eq!(lines[5], "const label = 'hello';");
        assert_eq!(lines[6], "const count: number = 42;");
    }

    // Verifica que el extractor conserve el lenguaje de bloques mezclados.
    #[test]
    fn detects_ts_and_js_script_variants_and_multiple_blocks() {
        let sfc = r#"
<!-- <script lang="ts">const ignored = 1;</script> -->
<script>
export default { name: 'CustomerCard' };
</script>

<SCRIPT setup generic="T extends Record<string, any>" lang='tsx'>
const ok: boolean = true;
</SCRIPT>
"#;
        let blocks = extract_vue_scripts(sfc);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].lang, VueScriptLang::Js);
        assert_eq!(blocks[1].lang, VueScriptLang::Tsx);
    }

    // Comprueba que TSX conserve llamadas, lecturas y tipos en las distintas pasadas.
    #[test]
    fn extracts_calls_types_and_zod_from_vue_tsx_blocks() {
        let source = r#"<script setup lang="tsx">
interface User { fullName: string }
const UserSchema = z.object({ fullName: z.string() });
const response = await fetch('http://customer-service/users');
const user = (await response.json()) as User;
const view = <div>{user.fullName}</div>;
</script>"#;
        let extracted = extract_file("User.vue", source, &customer_services()).unwrap();
        assert_eq!(extracted.calls[0].line, 4);
        assert_eq!(extracted.field_reads[0].field_path, "fullName");
        assert_eq!(extracted.field_reads[0].line, 6);
        let types = crate::types::extract_type_definitions("User.vue", source).unwrap();
        assert!(types.iter().any(|ty| ty.name == "User"));
        let schemas = crate::zod::extract_zod_schemas("User.vue", source).unwrap();
        assert!(schemas.iter().any(|schema| schema.name == "UserSchema"));
    }

    #[test]
    fn extracts_http_call_and_field_read_with_exact_lines_from_vue_component() {
        let sfc = "<template>\n  <p>{{ fullName }}</p>\n</template>\n\n<script setup lang=\"ts\">\nimport { ref } from 'vue';\n\nasync function loadCustomer(id: string) {\n  const res = await fetch(`http://customer-service/customers/${id}`);\n  const customer = (await res.json()) as { fullName: string };\n  return customer.fullName;\n}\n</script>\n";

        let extracted = extract_file("src/components/CustomerCard.vue", sfc, &customer_services())
            .expect("valid Vue SFC should extract");

        assert_eq!(extracted.file_path, "src/components/CustomerCard.vue");
        assert_eq!(extracted.calls.len(), 1);
        assert_eq!(extracted.calls[0].id, 0);
        assert_eq!(extracted.calls[0].client, HttpClientKind::Fetch);
        assert_eq!(extracted.calls[0].service, "customer-service");
        assert_eq!(extracted.calls[0].method, HttpMethod::Get);
        assert_eq!(extracted.calls[0].normalized_path, "/customers/:id");
        assert_eq!(extracted.calls[0].line, 9);

        assert_eq!(extracted.field_reads.len(), 1);
        assert_eq!(extracted.field_reads[0].call_id, 0);
        assert_eq!(extracted.field_reads[0].field_path, "fullName");
        assert_eq!(extracted.field_reads[0].line, 11);
    }

    #[test]
    fn handles_vue_component_without_script_block_and_merges_multiple_script_blocks() {
        let template_only = "<template><div>Static</div></template>\n";
        let empty = extract_file("src/Static.vue", template_only, &customer_services())
            .expect("template-only Vue SFC should succeed");
        assert!(empty.calls.is_empty());
        assert!(empty.field_reads.is_empty());
        assert!(empty.unresolved_calls.is_empty());

        let dual_script = r#"<script lang="ts">
export async function firstCall(id: string) {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const data = await res.json();
  return data.email;
}
</script>

<script setup lang="ts">
async function secondCall(id: string) {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const customer = await res.json();
  return customer.fullName;
}
</script>
"#;
        let merged = extract_file("src/Dual.vue", dual_script, &customer_services())
            .expect("dual-script Vue SFC should extract");

        assert_eq!(merged.calls.len(), 2);
        assert_eq!(merged.calls[0].id, 0);
        assert_eq!(merged.calls[1].id, 1);

        let reads: Vec<(usize, &str)> = merged
            .field_reads
            .iter()
            .map(|r| (r.call_id, r.field_path.as_str()))
            .collect();
        assert!(reads.contains(&(0, "email")));
        assert!(reads.contains(&(1, "fullName")));
    }

    #[test]
    fn extracts_vue_template_expressions_nested_directives_v_for_and_exact_lines() {
        let sfc = concat!(
            "<!-- <template><p>{{ ignored.topLevel }}</p></template> -->\n", // line 1
            "<script setup lang=\"ts\">\n",                                  // line 2
            "const fake = '<template><p>{{ ignored.inScript }}</p></template>';\n", // line 3
            "</script>\n",                                                   // line 4
            "<template>\n",                                                  // line 5
            "  <!-- <template><p>{{ ignored.comment }}</p><span :title=\"ignored.attr\"></span></template> -->\n", // line 6
            "  <div v-if=\"user.active\">\n",          // line 7
            "    <template v-if=\"user.verified\">\n", // line 8
            "      <p>Año: {{ user.segundoApellido }}</p>\n", // line 9
            "      <span :title=\"user.fullName\" v-bind:data-email='user.email'></span>\n", // line 10
            "    </template>\n", // line 11
            "    <p v-else-if=\"user.pending\" v-show=\"user.visible\" v-text=\"user.label\" v-html=\"user.bio\"></p>\n", // line 12
            "    <input v-model.trim=\"user.nickname\" />\n", // line 13
            "    <ul>\n",                                     // line 14
            "      <li v-for=\"item in order.items\">{{ item.name }}</li>\n", // line 15
            "      <li v-for=\"(entry, idx) of order.entries\" :key='entry.id'>\n", // line 16
            "        {{\n",                                   // line 17
            "          user.trailingNote\n",                  // line 18
            "        }}\n",                                   // line 19
            "      </li>\n",                                  // line 20
            "    </ul>\n",                                    // line 21
            "  </div>\n",                                     // line 22
            "</template>\n",                                  // line 23
        );

        let exprs = extract_vue_template_expressions(sfc);
        let pairs: Vec<(&str, usize)> = exprs.iter().map(|e| (e.expr.as_str(), e.line)).collect();

        assert_eq!(
            pairs,
            vec![
                ("user.active", 7),
                ("user.verified", 8),
                ("user.segundoApellido", 9),
                ("user.fullName", 10),
                ("user.email", 10),
                ("user.pending", 12),
                ("user.visible", 12),
                ("user.label", 12),
                ("user.bio", 12),
                ("user.nickname", 13),
                ("order.items", 15),
                ("item.name", 15),
                ("order.entries", 16),
                ("entry.id", 16),
                ("user.trailingNote", 18),
            ]
        );

        let segundo = exprs
            .iter()
            .find(|e| e.expr == "user.segundoApellido")
            .expect("segundoApellido expression should be extracted");
        assert_eq!(segundo.line, 9);
        assert_eq!(segundo.snippet, "<p>Año: {{ user.segundoApellido }}</p>");

        let trailing = exprs
            .iter()
            .find(|e| e.expr == "user.trailingNote")
            .expect("multiline mustache should be extracted");
        assert_eq!(trailing.line, 18);
        assert_eq!(trailing.snippet, "user.trailingNote");
    }

    #[test]
    fn extracts_v_for_rhs_with_destructured_aliases_and_object_mustaches() {
        let sfc = concat!(
            "<template>\n", // line 1
            "  <div v-for=\"({ id, info }, index) in order.items\">{{ { label: user.name } }}</div>\n", // line 2
            "  <span :label=\"user.count > 0 ? user.firstName : '—'\"></span>\n", // line 3
            "</template>\n",                                                      // line 4
        );

        let exprs = extract_vue_template_expressions(sfc);
        let pairs: Vec<(&str, usize)> = exprs.iter().map(|e| (e.expr.as_str(), e.line)).collect();

        assert_eq!(
            pairs,
            vec![
                ("order.items", 2),
                ("{ label: user.name }", 2),
                ("user.count > 0 ? user.firstName : '—'", 3),
            ]
        );
    }
}
