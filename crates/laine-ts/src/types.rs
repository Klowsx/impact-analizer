use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Program, PropertyKey, TSInterfaceDeclaration, TSSignature, TSType, TSTypeAliasDeclaration,
    TSTypeName, TSTypeQuery, TSTypeReference,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use serde::{Deserialize, Serialize};

use crate::module_resolver::{
    ImportedSymbol, ModuleResolver, ModuleSymbols, resolve_imported_symbol,
};
use crate::vue::extract_vue_scripts;
use crate::zod::collect_zod_definitions;

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

/// Local type declarations and schema values collected from one module.
#[derive(Default)]
pub struct TypeContractModule {
    symbols: ModuleSymbols,
    types: BTreeMap<String, TypeContractDef>,
    inferred_schemas: BTreeMap<String, Vec<String>>,
    schemas: BTreeMap<String, TypeContractDef>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum ReferenceKind {
    Type,
    Schema,
}

impl TypeContractModule {
    /// Parses each script once and keeps type aliases and schema values in separate namespaces.
    pub fn extract(file: &str, source: &str) -> Result<Self, Vec<String>> {
        let mut module = Self::default();
        let mut local_schemas = BTreeMap::new();
        let mut collect_block = |code, source_type| -> Result<(), Vec<String>> {
            let allocator = Allocator::default();
            let program = parse_program(&allocator, code, source_type)?;
            module.symbols.collect(&program, true);
            for def in collect_type_definitions(file, code, &program) {
                module.types.entry(def.name.clone()).or_insert(def);
            }
            for (def, inferred) in collect_zod_definitions(file, code, &program, &mut local_schemas)
            {
                if inferred {
                    module.inferred_schemas.insert(def.name, def.extends);
                } else {
                    module.schemas.insert(def.name.clone(), def);
                }
            }
            Ok(())
        };
        if file.to_ascii_lowercase().ends_with(".vue") {
            let blocks = extract_vue_scripts(source);
            for block in &blocks {
                collect_block(&block.code, block.lang.source_type())?;
            }
        } else {
            let source_type =
                SourceType::from_path(Path::new(file)).unwrap_or_else(|_| SourceType::ts());
            collect_block(source, source_type)?;
        }
        Ok(module)
    }
}

/// A snapshot index keyed by declaring module and symbol.
#[derive(Default)]
pub struct TypeContractRegistry {
    modules: BTreeMap<String, TypeContractModule>,
}

impl TypeContractRegistry {
    /// Registers a module's local declarations and import/export bindings.
    pub fn insert(&mut self, file: String, module: TypeContractModule) {
        self.modules.insert(file, module);
    }

    /// Resolves a response contract from its use site, following each dependency in its own module.
    pub fn resolve_fields(
        &self,
        file: &str,
        expression: &str,
        is_schema: bool,
        resolver: &ModuleResolver,
    ) -> Vec<(String, TypeFieldDef)> {
        let references = if is_schema {
            vec![(expression.to_string(), ReferenceKind::Schema)]
        } else {
            collect_response_references(expression)
        };
        let mut pending: Vec<_> = references
            .into_iter()
            .map(|(name, kind)| (file.to_string(), name, kind))
            .collect();
        let mut visited = HashSet::new();
        let mut fields = Vec::new();
        while let Some((scope, name, kind)) = pending.pop() {
            let Some((module, symbol)) = self.resolve_symbol(&scope, &name, resolver) else {
                continue;
            };
            if !visited.insert((module.clone(), symbol.clone(), kind)) {
                continue;
            }
            let Some(contracts) = self.modules.get(&module) else {
                continue;
            };
            if kind == ReferenceKind::Type
                && let Some(parents) = contracts.inferred_schemas.get(&symbol)
            {
                pending.extend(
                    parents
                        .iter()
                        .map(|parent| (module.clone(), parent.clone(), ReferenceKind::Schema)),
                );
            }
            let definition = match kind {
                ReferenceKind::Type => contracts.types.get(&symbol),
                ReferenceKind::Schema => contracts.schemas.get(&symbol),
            };
            if let Some(definition) = definition {
                fields.extend(
                    definition
                        .fields
                        .iter()
                        .map(|field| (module.clone(), field.clone())),
                );
                pending.extend(
                    definition
                        .extends
                        .iter()
                        .map(|parent| (module.clone(), parent.clone(), kind)),
                );
            }
        }
        sort_type_fields(&mut fields);
        fields
    }

    /// Looks up the binding in the current module and resolves imports through the snapshot.
    fn resolve_symbol(
        &self,
        file: &str,
        name: &str,
        resolver: &ModuleResolver,
    ) -> Option<(String, String)> {
        let bindings = &self.modules.get(file)?.symbols;
        let resolve = |import| {
            resolve_imported_symbol(
                file,
                import,
                &|path| self.modules.get(path).map(|module| &module.symbols),
                &|source, specifier| resolver.resolve_import(source, specifier),
            )
        };
        if let Some((namespace, exported)) = name.split_once('.') {
            let specifier = bindings.namespace_imports.get(namespace)?;
            resolve(&ImportedSymbol {
                specifier: specifier.clone(),
                name: exported.into(),
            })
        } else if let Some(import) = bindings.imports.get(name) {
            resolve(import)
        } else {
            Some((file.into(), name.into()))
        }
    }
}

/// Gets type and schema references from a response annotation without treating properties as types.
fn collect_response_references(expression: &str) -> Vec<(String, ReferenceKind)> {
    if let Some((first, rest)) = expression.as_bytes().split_first()
        && is_ident_start(*first)
        && rest.iter().copied().all(is_ident_continue)
    {
        return vec![(expression.into(), ReferenceKind::Type)];
    }
    let allocator = Allocator::default();
    let source = format!("type ResponseContract = {expression};");
    let Ok(program) = parse_program(&allocator, &source, SourceType::ts()) else {
        return Vec::new();
    };
    #[derive(Default)]
    struct References(Vec<(String, ReferenceKind)>);
    impl<'a> Visit<'a> for References {
        /// Collects a named type and visits generic arguments for additional dependencies.
        fn visit_ts_type_reference(&mut self, reference: &TSTypeReference<'a>) {
            if let Some(name) = ts_type_name(&reference.type_name) {
                self.0.push((name, ReferenceKind::Type));
            }
            walk::walk_ts_type_reference(self, reference);
        }

        /// Records a schema value referenced by an inferred response type.
        fn visit_ts_type_query(&mut self, query: &TSTypeQuery<'a>) {
            if let Some(name) = query.expr_name.as_ts_type_name().and_then(ts_type_name) {
                self.0.push((name, ReferenceKind::Schema));
            }
        }
    }
    let mut references = References::default();
    references.visit_program(&program);
    references.0
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

    sort_type_fields(&mut resolved);
    resolved
}

/// Orders declaration evidence consistently and collapses duplicate dependency paths.
fn sort_type_fields(resolved: &mut Vec<(String, TypeFieldDef)>) {
    resolved.sort_by(|(file_a, field_a), (file_b, field_b)| {
        file_a
            .cmp(file_b)
            .then_with(|| field_a.field_path.cmp(&field_b.field_path))
            .then_with(|| field_a.line.cmp(&field_b.line))
            .then_with(|| field_a.snippet.cmp(&field_b.snippet))
    });
    resolved.dedup();
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

/// Parses a script block and collects its TypeScript declarations.
fn extract_types_from_block(
    file_path: &str,
    source: &str,
    source_type: SourceType,
) -> Result<Vec<TypeContractDef>, Vec<String>> {
    let allocator = Allocator::default();
    let program = parse_program(&allocator, source, source_type)?;
    Ok(collect_type_definitions(file_path, source, &program))
}

/// Parses one script in its original language and returns its diagnostics on failure.
fn parse_program<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_type: SourceType,
) -> Result<Program<'a>, Vec<String>> {
    let parsed = Parser::new(allocator, source, source_type).parse();

    if !parsed.diagnostics.is_empty() {
        return Err(parsed
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    Ok(parsed.program)
}

/// Collects TypeScript contracts and their declaration locations.
fn collect_type_definitions(
    file_path: &str,
    source: &str,
    program: &Program<'_>,
) -> Vec<TypeContractDef> {
    let mut collector = TypeCollector {
        file_path,
        source,
        line_index: LineIndex::new(source),
        defs: Vec::new(),
    };
    collector.visit_program(program);
    collector.defs
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

    /// Collects fields and scoped references from a type alias.
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
                    if is_transparent_utility_wrapper(&name) {
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
                    } else if !is_ignored_builtin_type(&name) {
                        if seen_extends.insert(name.clone()) {
                            extends.push(name);
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
    /// Captures interface fields and the qualified names of their parents.
    fn visit_ts_interface_declaration(&mut self, decl: &TSInterfaceDeclaration<'a>) {
        let name = decl.id.name.to_string();
        let line = self.line_index.line_number(decl.span.start);

        let mut extends = Vec::new();
        let mut seen_extends = HashSet::new();
        for heritage in &decl.extends {
            if let Some(parent_name) = ts_type_name(&heritage.type_name) {
                if is_transparent_utility_wrapper(&parent_name) {
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
                } else if !is_ignored_builtin_type(&parent_name)
                    && seen_extends.insert(parent_name.clone())
                {
                    extends.push(parent_name);
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

/// Keeps qualified references intact so the module registry can identify namespace imports.
pub(crate) fn ts_type_name(name: &TSTypeName<'_>) -> Option<String> {
    match name {
        TSTypeName::IdentifierReference(id) => Some(id.name.to_string()),
        TSTypeName::QualifiedName(qn) => {
            Some(format!("{}.{}", ts_type_name(&qn.left)?, qn.right.name))
        }
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
