/// An extracted `<script>` block from a Vue Single File Component (`.vue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VueScriptBlock {
    /// Script source with leading `\n` padding so 1-based line numbers match the original `.vue` file.
    pub code: String,
    /// `true` when the opening `<script>` tag specifies `lang="ts"` / `lang="tsx"` (single or double quotes).
    pub is_ts: bool,
}

/// Extracts every `<script ...>...</script>` block from a `.vue` file.
///
/// Exact 1-based line numbers from the original `.vue` file are preserved by prepending
/// the exact number of `\n` characters that appeared before each script body.
pub fn extract_vue_scripts(source: &str) -> Vec<VueScriptBlock> {
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

        if lower[pos..].starts_with("<script") {
            let after_name = pos + "<script".len();
            if after_name < bytes.len()
                && matches!(
                    bytes[after_name],
                    b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n'
                )
            {
                let Some(tag_end) = find_opening_tag_end(bytes, after_name) else {
                    break;
                };
                let attrs = &lower[after_name..tag_end];
                if attrs.trim_end().ends_with('/') {
                    // Self-closing `<script src="..." />` has no inline body.
                    cursor = tag_end + 1;
                    continue;
                }

                let is_ts = opening_tag_is_ts(attrs);
                let body_start = tag_end + 1;
                let Some((body_end, after_close)) = find_closing_script_tag(&lower, body_start)
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

                blocks.push(VueScriptBlock { code, is_ts });
                cursor = after_close;
                continue;
            }
        }

        cursor = pos + 1;
    }

    blocks
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

fn find_closing_script_tag(lower: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = lower.as_bytes();
    let mut search = start;

    while search < bytes.len() {
        let rel = lower[search..].find("</script")?;
        let close_pos = search + rel;
        let mut idx = close_pos + "</script".len();

        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx < bytes.len() && bytes[idx] == b'>' {
            return Some((close_pos, idx + 1));
        }

        search = close_pos + "</script".len();
    }

    None
}

fn opening_tag_is_ts(attrs: &str) -> bool {
    let bytes = attrs.as_bytes();
    let mut i = 0;

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
                return matches!(value.trim(), "ts" | "tsx");
            }
        }
    }

    false
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
        assert!(blocks[0].is_ts);

        let lines: Vec<&str> = blocks[0].code.lines().collect();
        // Lines 1..=5 are empty (template + blank + opening `<script>` line), line 6 is `const label = 'hello';`.
        assert_eq!(lines[5], "const label = 'hello';");
        assert_eq!(lines[6], "const count: number = 42;");
    }

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
        assert!(!blocks[0].is_ts);
        assert!(blocks[1].is_ts);
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
}
