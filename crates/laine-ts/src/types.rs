use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    PropertyKey, TSInterfaceDeclaration, TSSignature, TSType, TSTypeAliasDeclaration, TSTypeName,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::vue::extract_vue_scripts;

/// A field declared inside a TypeScript `interface` or `type` contract definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeFieldDef {
    pub field_path: String,
    pub line: usize,
    pub snippet: String,
}

/// A TypeScript `interface` or `type` contract extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeContractDef {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub extends: Vec<String>,
    pub fields: Vec<TypeFieldDef>,
}

/// Extracts TypeScript `interface` and object/intersection/union `type` alias definitions
/// from a `.ts`, `.tsx`, `.mts`, `.cts`, `.js`, or `.vue` file.
// Extrae tipos con el parser adecuado para cada bloque de script.
pub fn extract_type_definitions(
    file_path: &str,
    source: &str,
) -> Result<Vec<TypeContractDef>, Vec<String>> {
    if file_path.to_ascii_lowercase().ends_with(".vue") {
        let blocks = extract_vue_scripts(source);
        let mut defs = Vec::new();
        for block in blocks {
            let source_type = block.lang.source_type();
            defs.extend(extract_types_from_block(
                file_path,
                &block.code,
                source_type,
            )?);
        }
        return Ok(defs);
    }

    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::ts());
    extract_types_from_block(file_path, source, source_type)
}

/// Resolves all fields reachable from a TypeScript type expression (such as `"CustomerDTO"`,
/// `"PreSyncResponse | PostSyncResponse"`, `"Promise<CustomerDTO>"`, `"CustomerDTO[]"`,
/// `"Array<CustomerDTO>"`, or `"ApiEnvelope<CustomerDTO>"`) against a type registry.
///
/// Follows `extends` links recursively with cycle protection and returns deduplicated
/// `(defining_file_path, TypeFieldDef)` pairs sorted deterministically.
pub fn resolve_type_fields(
    type_expr: &str,
    registry: &BTreeMap<String, TypeContractDef>,
) -> Vec<(String, TypeFieldDef)> {
    let mut visited = HashSet::new();
    let mut resolved = Vec::new();

    for ident in extract_type_identifiers(type_expr) {
        collect_resolved_fields(ident, registry, &mut visited, &mut resolved);
    }

    resolved.sort_by(|(file_a, field_a), (file_b, field_b)| {
        file_a
            .cmp(file_b)
            .then_with(|| field_a.field_path.cmp(&field_b.field_path))
            .then_with(|| field_a.line.cmp(&field_b.line))
            .then_with(|| field_a.snippet.cmp(&field_b.snippet))
    });
    resolved.dedup();
    resolved
}

fn collect_resolved_fields(
    type_name: &str,
    registry: &BTreeMap<String, TypeContractDef>,
    visited: &mut HashSet<String>,
    out: &mut Vec<(String, TypeFieldDef)>,
) {
    if !visited.insert(type_name.to_string()) {
        return;
    }

    let Some(contract) = registry.get(type_name) else {
        return;
    };

    for field in &contract.fields {
        out.push((contract.file.clone(), field.clone()));
    }

    for parent in &contract.extends {
        collect_resolved_fields(parent, registry, visited, out);
    }
}

fn extract_type_identifiers(type_expr: &str) -> Vec<&str> {
    let bytes = type_expr.as_bytes();
    let mut idents = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            idents.push(&type_expr[start..i]);
        } else {
            i += 1;
        }
    }

    idents
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn extract_types_from_block(
    file_path: &str,
    source: &str,
    source_type: SourceType,
) -> Result<Vec<TypeContractDef>, Vec<String>> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if !parsed.diagnostics.is_empty() {
        return Err(parsed
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    let mut collector = TypeCollector {
        file_path,
        source,
        line_index: LineIndex::new(source),
        defs: Vec::new(),
    };
    collector.visit_program(&parsed.program);
    Ok(collector.defs)
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

struct TypeCollector<'s> {
    file_path: &'s str,
    source: &'s str,
    line_index: LineIndex,
    defs: Vec<TypeContractDef>,
}

impl<'s> TypeCollector<'s> {
    fn location(&self, span: Span) -> (usize, String) {
        let line = self.line_index.line_number(span.start);
        let snippet = self.line_index.line_snippet(self.source, line).to_string();
        (line, snippet)
    }

    fn collect_signatures(
        &self,
        members: &[TSSignature<'_>],
        prefix: &str,
        fields: &mut Vec<TypeFieldDef>,
        seen_fields: &mut HashSet<(String, usize)>,
    ) {
        for member in members {
            let TSSignature::TSPropertySignature(prop) = member else {
                continue;
            };
            let Some(key) = property_key_name(&prop.key) else {
                continue;
            };
            let field_path = join_field_path(prefix, &key);
            let (line, snippet) = self.location(prop.span);

            if seen_fields.insert((field_path.clone(), line)) {
                fields.push(TypeFieldDef {
                    field_path: field_path.clone(),
                    line,
                    snippet,
                });
            }

            if let Some(type_ann) = &prop.type_annotation {
                self.collect_nested_type_fields(
                    &type_ann.type_annotation,
                    &field_path,
                    fields,
                    seen_fields,
                );
            }
        }
    }

    fn collect_nested_type_fields(
        &self,
        ts_type: &TSType<'_>,
        prefix: &str,
        fields: &mut Vec<TypeFieldDef>,
        seen_fields: &mut HashSet<(String, usize)>,
    ) {
        match ts_type {
            TSType::TSTypeLiteral(lit) => {
                self.collect_signatures(&lit.members, prefix, fields, seen_fields);
            }
            TSType::TSParenthesizedType(paren) => {
                self.collect_nested_type_fields(
                    &paren.type_annotation,
                    prefix,
                    fields,
                    seen_fields,
                );
            }
            TSType::TSUnionType(union) => {
                for member in &union.types {
                    self.collect_nested_type_fields(member, prefix, fields, seen_fields);
                }
            }
            TSType::TSIntersectionType(inter) => {
                for member in &inter.types {
                    self.collect_nested_type_fields(member, prefix, fields, seen_fields);
                }
            }
            _ => {}
        }
    }

    fn collect_alias_body(
        &self,
        ts_type: &TSType<'_>,
        extends: &mut Vec<String>,
        seen_extends: &mut HashSet<String>,
        fields: &mut Vec<TypeFieldDef>,
        seen_fields: &mut HashSet<(String, usize)>,
        is_object_like: &mut bool,
    ) {
        match ts_type {
            TSType::TSTypeLiteral(lit) => {
                *is_object_like = true;
                self.collect_signatures(&lit.members, "", fields, seen_fields);
            }
            TSType::TSIntersectionType(inter) => {
                for member in &inter.types {
                    self.collect_alias_body(
                        member,
                        extends,
                        seen_extends,
                        fields,
                        seen_fields,
                        is_object_like,
                    );
                }
            }
            TSType::TSUnionType(union) => {
                for member in &union.types {
                    self.collect_alias_body(
                        member,
                        extends,
                        seen_extends,
                        fields,
                        seen_fields,
                        is_object_like,
                    );
                }
            }
            TSType::TSParenthesizedType(paren) => {
                self.collect_alias_body(
                    &paren.type_annotation,
                    extends,
                    seen_extends,
                    fields,
                    seen_fields,
                    is_object_like,
                );
            }
            TSType::TSArrayType(arr) => {
                self.collect_alias_body(
                    &arr.element_type,
                    extends,
                    seen_extends,
                    fields,
                    seen_fields,
                    is_object_like,
                );
            }
            TSType::TSTypeReference(type_ref) => {
                if let Some(name) = ts_type_name(&type_ref.type_name) {
                    if is_transparent_utility_wrapper(name) {
                        if let Some(args) = &type_ref.type_arguments
                            && let Some(first_arg) = args.params.first()
                        {
                            self.collect_alias_body(
                                first_arg,
                                extends,
                                seen_extends,
                                fields,
                                seen_fields,
                                is_object_like,
                            );
                        }
                    } else if !is_ignored_builtin_type(name) {
                        if seen_extends.insert(name.to_string()) {
                            extends.push(name.to_string());
                        }
                        if let Some(args) = &type_ref.type_arguments {
                            for param in &args.params {
                                self.collect_alias_body(
                                    param,
                                    extends,
                                    seen_extends,
                                    fields,
                                    seen_fields,
                                    is_object_like,
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for TypeCollector<'_> {
    fn visit_ts_interface_declaration(&mut self, decl: &TSInterfaceDeclaration<'a>) {
        let name = decl.id.name.to_string();
        let line = self.line_index.line_number(decl.span.start);

        let mut extends = Vec::new();
        let mut seen_extends = HashSet::new();
        for heritage in &decl.extends {
            if let Some(parent_name) = ts_type_name(&heritage.type_name) {
                if is_transparent_utility_wrapper(parent_name) {
                    if let Some(args) = &heritage.type_arguments
                        && let Some(first_arg) = args.params.first()
                    {
                        let mut dummy_fields = Vec::new();
                        let mut dummy_seen = HashSet::new();
                        let mut dummy_obj = false;
                        self.collect_alias_body(
                            first_arg,
                            &mut extends,
                            &mut seen_extends,
                            &mut dummy_fields,
                            &mut dummy_seen,
                            &mut dummy_obj,
                        );
                    }
                } else if !is_ignored_builtin_type(parent_name)
                    && seen_extends.insert(parent_name.to_string())
                {
                    extends.push(parent_name.to_string());
                }
            }
        }

        let mut fields = Vec::new();
        let mut seen_fields = HashSet::new();
        self.collect_signatures(&decl.body.body, "", &mut fields, &mut seen_fields);

        self.defs.push(TypeContractDef {
            name,
            file: self.file_path.to_string(),
            line,
            extends,
            fields,
        });

        walk::walk_ts_interface_declaration(self, decl);
    }

    fn visit_ts_type_alias_declaration(&mut self, decl: &TSTypeAliasDeclaration<'a>) {
        let name = decl.id.name.to_string();
        let line = self.line_index.line_number(decl.span.start);

        let mut extends = Vec::new();
        let mut seen_extends = HashSet::new();
        let mut fields = Vec::new();
        let mut seen_fields = HashSet::new();
        let mut is_object_like = false;

        self.collect_alias_body(
            &decl.type_annotation,
            &mut extends,
            &mut seen_extends,
            &mut fields,
            &mut seen_fields,
            &mut is_object_like,
        );

        if is_object_like || !fields.is_empty() || !extends.is_empty() {
            self.defs.push(TypeContractDef {
                name,
                file: self.file_path.to_string(),
                line,
                extends,
                fields,
            });
        }

        walk::walk_ts_type_alias_declaration(self, decl);
    }
}

fn ts_type_name<'a>(name: &'a TSTypeName<'a>) -> Option<&'a str> {
    match name {
        TSTypeName::IdentifierReference(id) => Some(id.name.as_str()),
        TSTypeName::QualifiedName(qn) => Some(qn.right.name.as_str()),
        TSTypeName::ThisExpression(_) => None,
    }
}

fn is_transparent_utility_wrapper(name: &str) -> bool {
    matches!(
        name,
        "Partial"
            | "Required"
            | "Readonly"
            | "Pick"
            | "Omit"
            | "Array"
            | "ReadonlyArray"
            | "Promise"
            | "Awaited"
            | "NonNullable"
    )
}

fn is_ignored_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "Record"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "Exclude"
            | "Extract"
            | "Date"
            | "RegExp"
    )
}

fn property_key_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(ident) => Some(ident.name.to_string()),
        PropertyKey::StringLiteral(lit) => lit.value.as_str().map(ToOwned::to_owned),
        _ => None,
    }
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
    use super::*;

    #[test]
    fn extracts_interface_extends_and_nested_object_fields() {
        let code = r#"export interface BaseEntity {
  id: number;
}

export interface CustomerDTO extends BaseEntity {
  fullName: string;
  address: {
    city: string;
    geo?: {
      lat: number;
    };
  };
}
"#;

        let defs =
            extract_type_definitions("src/types/customer.ts", code).expect("valid TS should parse");
        assert_eq!(defs.len(), 2);

        let base = &defs[0];
        assert_eq!(base.name, "BaseEntity");
        assert_eq!(base.file, "src/types/customer.ts");
        assert_eq!(base.line, 1);
        assert!(base.extends.is_empty());
        assert_eq!(base.fields.len(), 1);
        assert_eq!(base.fields[0].field_path, "id");
        assert_eq!(base.fields[0].line, 2);
        assert_eq!(base.fields[0].snippet, "id: number;");

        let customer = &defs[1];
        assert_eq!(customer.name, "CustomerDTO");
        assert_eq!(customer.line, 5);
        assert_eq!(customer.extends, vec!["BaseEntity"]);

        let paths: Vec<(&str, usize)> = customer
            .fields
            .iter()
            .map(|f| (f.field_path.as_str(), f.line))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("fullName", 6),
                ("address", 7),
                ("address.city", 8),
                ("address.geo", 9),
                ("address.geo.lat", 10),
            ]
        );
    }

    #[test]
    fn extracts_object_intersection_and_union_type_aliases() {
        let code = r#"export type PreSyncResponse = {
  status: string;
};

export type PostSyncResponse = PreSyncResponse & {
  syncedAt: string;
  meta: {
    durationMs: number;
  };
};

export type SyncResponse = PreSyncResponse | PostSyncResponse;
export type StatusLabel = "pending" | "done";
"#;

        let defs =
            extract_type_definitions("src/types/sync.ts", code).expect("valid TS should parse");
        assert_eq!(defs.len(), 3);

        assert_eq!(defs[0].name, "PreSyncResponse");
        assert_eq!(defs[0].fields.len(), 1);
        assert_eq!(defs[0].fields[0].field_path, "status");

        assert_eq!(defs[1].name, "PostSyncResponse");
        assert_eq!(defs[1].extends, vec!["PreSyncResponse"]);
        let post_fields: Vec<&str> = defs[1]
            .fields
            .iter()
            .map(|f| f.field_path.as_str())
            .collect();
        assert_eq!(post_fields, vec!["syncedAt", "meta", "meta.durationMs"]);

        assert_eq!(defs[2].name, "SyncResponse");
        assert_eq!(defs[2].extends, vec!["PreSyncResponse", "PostSyncResponse"]);
        assert!(defs[2].fields.is_empty());
    }

    #[test]
    fn extracts_types_from_vue_sfc_with_exact_line_numbers() {
        let sfc = "<template><div /></template>\n\n<script setup lang=\"ts\">\ninterface VueCustomer {\n  fullName: string;\n}\n</script>\n";
        let defs =
            extract_type_definitions("src/Customer.vue", sfc).expect("valid Vue SFC should parse");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "VueCustomer");
        assert_eq!(defs[0].line, 4);
        assert_eq!(defs[0].fields.len(), 1);
        assert_eq!(defs[0].fields[0].field_path, "fullName");
        assert_eq!(defs[0].fields[0].line, 5);
        assert_eq!(defs[0].fields[0].snippet, "fullName: string;");
    }

    #[test]
    fn resolves_union_generic_array_and_envelope_type_expressions() {
        let base_code = r#"export interface BaseEntity {
  id: number;
}
"#;
        let customer_code = r#"export interface CustomerDTO extends BaseEntity {
  fullName: string;
}
export interface ApiEnvelope<T> {
  ok: boolean;
  data: T;
}
export interface PreSyncResponse {
  status: string;
}
export interface PostSyncResponse {
  syncedAt: string;
}
"#;

        let mut registry = BTreeMap::new();
        for def in extract_type_definitions("src/types/base.ts", base_code).unwrap() {
            registry.insert(def.name.clone(), def);
        }
        for def in extract_type_definitions("src/types/customer.ts", customer_code).unwrap() {
            registry.insert(def.name.clone(), def);
        }

        // Direct + inherited field from another file
        let direct = resolve_type_fields("CustomerDTO", &registry);
        assert_eq!(
            direct
                .iter()
                .map(|(file, f)| (file.as_str(), f.field_path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("src/types/base.ts", "id"),
                ("src/types/customer.ts", "fullName"),
            ]
        );

        // Union type expression
        let union = resolve_type_fields("PreSyncResponse | PostSyncResponse", &registry);
        assert_eq!(
            union
                .iter()
                .map(|(_, f)| f.field_path.as_str())
                .collect::<Vec<_>>(),
            vec!["status", "syncedAt"]
        );

        // Wrapper generics & arrays
        for expr in [
            "Promise<CustomerDTO>",
            "CustomerDTO[]",
            "Array<CustomerDTO>",
        ] {
            let resolved = resolve_type_fields(expr, &registry);
            assert_eq!(resolved.len(), 2, "failed for {expr}");
            assert_eq!(resolved[0].1.field_path, "id");
            assert_eq!(resolved[1].1.field_path, "fullName");
        }

        // Envelope generic resolves both outer envelope and inner payload fields
        let envelope = resolve_type_fields("ApiEnvelope<CustomerDTO>", &registry);
        assert_eq!(
            envelope
                .iter()
                .map(|(file, f)| (file.as_str(), f.field_path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("src/types/base.ts", "id"),
                ("src/types/customer.ts", "data"),
                ("src/types/customer.ts", "fullName"),
                ("src/types/customer.ts", "ok"),
            ]
        );
    }

    #[test]
    fn resolves_cyclic_extends_without_infinite_recursion() {
        let mut registry = BTreeMap::new();
        registry.insert(
            "NodeA".into(),
            TypeContractDef {
                name: "NodeA".into(),
                file: "src/a.ts".into(),
                line: 1,
                extends: vec!["NodeB".into()],
                fields: vec![TypeFieldDef {
                    field_path: "alpha".into(),
                    line: 2,
                    snippet: "alpha: string;".into(),
                }],
            },
        );
        registry.insert(
            "NodeB".into(),
            TypeContractDef {
                name: "NodeB".into(),
                file: "src/b.ts".into(),
                line: 1,
                extends: vec!["NodeA".into()],
                fields: vec![TypeFieldDef {
                    field_path: "beta".into(),
                    line: 2,
                    snippet: "beta: number;".into(),
                }],
            },
        );

        let resolved = resolve_type_fields("NodeA", &registry);
        assert_eq!(
            resolved
                .iter()
                .map(|(file, f)| (file.as_str(), f.field_path.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/a.ts", "alpha"), ("src/b.ts", "beta")]
        );
    }
}
