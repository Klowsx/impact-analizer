use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrowFunctionBody, BindingPattern, CallExpression, Expression, ObjectExpression,
    ObjectPropertyKind, PropertyKey, Statement, TSType, TSTypeAliasDeclaration, TSTypeName,
    VariableDeclarator,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

use crate::types::{TypeContractDef, TypeFieldDef};
use crate::vue::extract_vue_scripts;

/// Extracts Zod runtime object schemas (`z.object({...})`, chained `.extend(...)` / `.merge(...)` /
/// `.pick(...)` / `.omit(...)` / modifiers) and inferred type aliases
/// (`type User = z.infer<typeof UserSchema>`, `z.output<...>`, `z.input<...>`)
/// from a `.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs`, or `.vue` file.
pub fn extract_zod_schemas(
    file_path: &str,
    source: &str,
) -> Result<Vec<TypeContractDef>, Vec<String>> {
    let mut local_schemas = BTreeMap::new();

    if file_path.to_ascii_lowercase().ends_with(".vue") {
        let blocks = extract_vue_scripts(source);
        let mut defs = Vec::new();
        for block in blocks {
            let source_type = if block.is_ts {
                SourceType::ts()
            } else {
                SourceType::mjs()
            };
            defs.extend(extract_zod_from_block(
                file_path,
                &block.code,
                source_type,
                &mut local_schemas,
            )?);
        }
        return Ok(defs);
    }

    let source_type =
        SourceType::from_path(Path::new(file_path)).unwrap_or_else(|_| SourceType::ts());
    extract_zod_from_block(file_path, source, source_type, &mut local_schemas)
}

/// Recognizes Zod parsing calls (`Schema.parse(...)`, `Schema.safeParse(...)`,
/// `Schema.parseAsync(...)`, `Schema.safeParseAsync(...)`, including wrapped forms such as
/// `Schema.array().parse(...)`, `Schema.partial().safeParse(...)`, or `z.array(Schema).parse(...)`)
/// and returns the referenced schema identifier name.
pub fn extract_zod_parse_schema_name(call: &CallExpression<'_>) -> Option<String> {
    let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) else {
        return None;
    };

    if !matches!(
        member.property.name.as_str(),
        "parse" | "safeParse" | "parseAsync" | "safeParseAsync"
    ) {
        return None;
    }

    extract_parse_target_schema_name(&member.object)
}

fn extract_parse_target_schema_name(expr: &Expression<'_>) -> Option<String> {
    match unwrap_expr(expr) {
        Expression::Identifier(ident) => {
            let name = ident.name.as_str();
            if is_non_zod_parse_builtin(name) {
                None
            } else {
                Some(name.to_string())
            }
        }
        Expression::StaticMemberExpression(member) => {
            let name = member.property.name.as_str();
            if is_non_zod_parse_builtin(name) {
                None
            } else {
                Some(name.to_string())
            }
        }
        Expression::CallExpression(inner_call) => {
            let Expression::StaticMemberExpression(inner_member) = unwrap_expr(&inner_call.callee)
            else {
                return None;
            };
            let method = inner_member.property.name.as_str();
            if is_zod_namespace(&inner_member.object) {
                if matches!(
                    method,
                    "array" | "optional" | "nullable" | "readonly" | "nonoptional" | "catch"
                ) {
                    let arg_expr = inner_call.arguments.first()?.as_expression()?;
                    return extract_parse_target_schema_name(arg_expr);
                }
                if method == "preprocess" {
                    let arg_expr = inner_call.arguments.get(1)?.as_expression()?;
                    return extract_parse_target_schema_name(arg_expr);
                }
                return None;
            }
            if is_transparent_zod_modifier(method) || method == "array" {
                return extract_parse_target_schema_name(&inner_member.object);
            }
            None
        }
        _ => None,
    }
}

fn extract_zod_from_block(
    file_path: &str,
    source: &str,
    source_type: SourceType,
    local_schemas: &mut BTreeMap<String, TypeContractDef>,
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

    let mut collector = ZodCollector {
        file_path,
        source,
        line_index: LineIndex::new(source),
        local_schemas,
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

#[derive(Debug, Clone, Default)]
struct EvaluatedSchema {
    extends: Vec<String>,
    fields: Vec<TypeFieldDef>,
}

impl EvaluatedSchema {
    fn merge_from(&mut self, other: EvaluatedSchema) {
        for parent in other.extends {
            push_unique_extend(&mut self.extends, parent);
        }
        for field in other.fields {
            push_unique_field(&mut self.fields, field);
        }
    }
}

struct ZodCollector<'s> {
    file_path: &'s str,
    source: &'s str,
    line_index: LineIndex,
    local_schemas: &'s mut BTreeMap<String, TypeContractDef>,
    defs: Vec<TypeContractDef>,
}

impl ZodCollector<'_> {
    fn location(&self, span: Span) -> (usize, String) {
        let line = self.line_index.line_number(span.start);
        let snippet = self.line_index.line_snippet(self.source, line).to_string();
        (line, snippet)
    }

    fn evaluate_schema_expr(&self, expr: &Expression<'_>) -> Option<EvaluatedSchema> {
        let unwrapped = unwrap_expr(expr);
        match unwrapped {
            Expression::Identifier(ident) => {
                let name = ident.name.as_str();
                if self.local_schemas.contains_key(name) {
                    Some(EvaluatedSchema {
                        extends: vec![name.to_string()],
                        fields: Vec::new(),
                    })
                } else {
                    None
                }
            }
            Expression::CallExpression(call) => self.evaluate_call_schema(call),
            _ => None,
        }
    }

    fn evaluate_schema_or_ref(&self, expr: &Expression<'_>) -> Option<EvaluatedSchema> {
        if let Some(schema) = self.evaluate_schema_expr(expr) {
            return Some(schema);
        }
        extract_schema_ident(expr).map(|ident| EvaluatedSchema {
            extends: vec![ident],
            fields: Vec::new(),
        })
    }

    fn evaluate_call_schema(&self, call: &CallExpression<'_>) -> Option<EvaluatedSchema> {
        let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) else {
            return None;
        };
        let method = member.property.name.as_str();

        if is_zod_namespace(&member.object) {
            return self.evaluate_zod_static_call(method, call);
        }

        self.evaluate_zod_chained_call(&member.object, method, call)
    }

    fn evaluate_zod_static_call(
        &self,
        method: &str,
        call: &CallExpression<'_>,
    ) -> Option<EvaluatedSchema> {
        match method {
            "object" | "strictObject" | "looseObject" => {
                let arg_expr = call.arguments.first()?.as_expression()?;
                match unwrap_expr(arg_expr) {
                    Expression::ObjectExpression(obj) => {
                        let mut schema = EvaluatedSchema::default();
                        self.collect_object_properties(
                            obj,
                            "",
                            &mut schema.extends,
                            &mut schema.fields,
                        );
                        Some(schema)
                    }
                    other => extract_shape_owner(other).map(|owner| EvaluatedSchema {
                        extends: vec![owner],
                        fields: Vec::new(),
                    }),
                }
            }
            "array" | "optional" | "nullable" | "readonly" | "nonoptional" | "catch" => {
                let arg_expr = call.arguments.first()?.as_expression()?;
                self.evaluate_schema_or_ref(arg_expr)
            }
            "preprocess" => {
                let arg_expr = call.arguments.get(1)?.as_expression()?;
                self.evaluate_schema_or_ref(arg_expr)
            }
            "lazy" => {
                let arg_expr = call.arguments.first()?.as_expression()?;
                let returned = extract_lazy_return_expr(arg_expr)?;
                self.evaluate_schema_or_ref(returned)
            }
            "intersection" => {
                let left = call.arguments.first()?.as_expression()?;
                let right = call.arguments.get(1)?.as_expression()?;
                let mut combined = EvaluatedSchema::default();
                let mut found = false;
                if let Some(s) = self.evaluate_schema_or_ref(left) {
                    combined.merge_from(s);
                    found = true;
                }
                if let Some(s) = self.evaluate_schema_or_ref(right) {
                    combined.merge_from(s);
                    found = true;
                }
                found.then_some(combined)
            }
            "union" | "xor" => {
                let arg_expr = call.arguments.first()?.as_expression()?;
                self.evaluate_schema_array_arg(arg_expr)
            }
            "discriminatedUnion" => {
                let arg_expr = call.arguments.get(1)?.as_expression()?;
                self.evaluate_schema_array_arg(arg_expr)
            }
            _ => None,
        }
    }

    fn evaluate_schema_array_arg(&self, expr: &Expression<'_>) -> Option<EvaluatedSchema> {
        let Expression::ArrayExpression(arr) = unwrap_expr(expr) else {
            return None;
        };
        let mut combined = EvaluatedSchema::default();
        let mut found = false;
        for elem in &arr.elements {
            if let Some(elem_expr) = elem.as_expression()
                && let Some(schema) = self.evaluate_schema_or_ref(elem_expr)
            {
                combined.merge_from(schema);
                found = true;
            }
        }
        found.then_some(combined)
    }

    fn evaluate_zod_chained_call(
        &self,
        receiver: &Expression<'_>,
        method: &str,
        call: &CallExpression<'_>,
    ) -> Option<EvaluatedSchema> {
        match method {
            "extend" => {
                if call.arguments.len() != 1 {
                    return None;
                }
                let arg_expr = call.arguments.first()?.as_expression()?;
                let mut base = self.evaluate_schema_or_ref(receiver)?;
                match unwrap_expr(arg_expr) {
                    Expression::ObjectExpression(obj) => {
                        if !has_non_method_or_spread_property(obj)
                            && self.evaluate_schema_expr(receiver).is_none()
                        {
                            return None;
                        }
                        self.collect_object_properties(
                            obj,
                            "",
                            &mut base.extends,
                            &mut base.fields,
                        );
                        Some(base)
                    }
                    other => {
                        let owner = extract_shape_owner(other)?;
                        push_unique_extend(&mut base.extends, owner);
                        Some(base)
                    }
                }
            }
            "merge" => {
                if call.arguments.len() != 1 {
                    return None;
                }
                let arg_expr = call.arguments.first()?.as_expression()?;
                let mut left = self.evaluate_schema_or_ref(receiver)?;
                let right = self.evaluate_schema_or_ref(arg_expr)?;
                left.merge_from(right);
                Some(left)
            }
            "and" | "or" => {
                if call.arguments.len() != 1 {
                    return None;
                }
                let arg_expr = call.arguments.first()?.as_expression()?;
                let left_known = self.evaluate_schema_expr(receiver);
                let right_known = self.evaluate_schema_expr(arg_expr);
                if left_known.is_none() && right_known.is_none() {
                    return None;
                }
                let mut left = left_known.or_else(|| self.evaluate_schema_or_ref(receiver))?;
                let right = right_known.or_else(|| self.evaluate_schema_or_ref(arg_expr))?;
                left.merge_from(right);
                Some(left)
            }
            "pick" => {
                if call.arguments.len() != 1 {
                    return None;
                }
                let arg_expr = call.arguments.first()?.as_expression()?;
                let receiver_known = self.evaluate_schema_expr(receiver);
                let Expression::ObjectExpression(mask_obj) = unwrap_expr(arg_expr) else {
                    return receiver_known;
                };
                if receiver_known.is_none() && !is_zod_boolean_mask(mask_obj) {
                    return None;
                }
                let base = receiver_known.or_else(|| self.evaluate_schema_or_ref(receiver))?;
                let (base_fields, _) = self.expand_evaluated_schema(&base);

                let mut picked_fields = Vec::new();
                for prop_kind in &mask_obj.properties {
                    let ObjectPropertyKind::ObjectProperty(prop) = prop_kind else {
                        continue;
                    };
                    if prop.method || !is_truthy_mask_value(&prop.value) {
                        continue;
                    }
                    let Some(key) = property_key_name(&prop.key) else {
                        continue;
                    };
                    let (line, snippet) = self.location(prop.span);
                    push_unique_field(
                        &mut picked_fields,
                        TypeFieldDef {
                            field_path: key.clone(),
                            line,
                            snippet,
                        },
                    );
                    for descendant in &base_fields {
                        if is_descendant_field(&descendant.field_path, &key) {
                            push_unique_field(&mut picked_fields, descendant.clone());
                        }
                    }
                }

                Some(EvaluatedSchema {
                    extends: Vec::new(),
                    fields: picked_fields,
                })
            }
            "omit" => {
                if call.arguments.len() != 1 {
                    return None;
                }
                let arg_expr = call.arguments.first()?.as_expression()?;
                let receiver_known = self.evaluate_schema_expr(receiver);
                let Expression::ObjectExpression(mask_obj) = unwrap_expr(arg_expr) else {
                    return receiver_known;
                };
                if receiver_known.is_none() && !is_zod_boolean_mask(mask_obj) {
                    return None;
                }
                let base = receiver_known.or_else(|| self.evaluate_schema_or_ref(receiver))?;

                let mut omitted_keys = HashSet::new();
                for prop_kind in &mask_obj.properties {
                    let ObjectPropertyKind::ObjectProperty(prop) = prop_kind else {
                        continue;
                    };
                    if prop.method || !is_truthy_mask_value(&prop.value) {
                        continue;
                    }
                    if let Some(key) = property_key_name(&prop.key) {
                        omitted_keys.insert(key);
                    }
                }

                let (expanded_fields, unresolved_extends) = self.expand_evaluated_schema(&base);
                let filtered_fields = expanded_fields
                    .into_iter()
                    .filter(|f| !is_field_omitted(&f.field_path, &omitted_keys))
                    .collect();

                Some(EvaluatedSchema {
                    extends: unresolved_extends,
                    fields: filtered_fields,
                })
            }
            m if is_transparent_zod_modifier(m) || m == "array" || m == "pipe" => {
                if let Some(schema) = self.evaluate_schema_expr(receiver) {
                    return Some(schema);
                }
                if is_unambiguous_zod_ident_modifier(m) {
                    return extract_schema_ident(receiver).map(|ident| EvaluatedSchema {
                        extends: vec![ident],
                        fields: Vec::new(),
                    });
                }
                None
            }
            _ => None,
        }
    }

    fn collect_object_properties(
        &self,
        obj: &ObjectExpression<'_>,
        prefix: &str,
        extends: &mut Vec<String>,
        fields: &mut Vec<TypeFieldDef>,
    ) {
        for prop_kind in &obj.properties {
            match prop_kind {
                ObjectPropertyKind::ObjectProperty(prop) => {
                    if prop.method {
                        continue;
                    }
                    let Some(key) = property_key_name(&prop.key) else {
                        continue;
                    };
                    let field_path = join_field_path(prefix, &key);
                    let (line, snippet) = self.location(prop.span);

                    push_unique_field(
                        fields,
                        TypeFieldDef {
                            field_path: field_path.clone(),
                            line,
                            snippet,
                        },
                    );

                    self.collect_nested_field_schema(&prop.value, &field_path, fields);
                }
                ObjectPropertyKind::SpreadProperty(spread) => {
                    if let Some(owner) = extract_shape_owner(&spread.argument) {
                        if prefix.is_empty() {
                            push_unique_extend(extends, owner);
                        } else {
                            self.expand_local_schema_fields(&owner, prefix, fields);
                        }
                    }
                }
            }
        }
    }

    fn collect_nested_field_schema(
        &self,
        expr: &Expression<'_>,
        prefix: &str,
        fields: &mut Vec<TypeFieldDef>,
    ) {
        match unwrap_expr(expr) {
            Expression::Identifier(ident) => {
                self.expand_local_schema_fields(ident.name.as_str(), prefix, fields);
            }
            Expression::CallExpression(call) => {
                let Expression::StaticMemberExpression(member) = unwrap_expr(&call.callee) else {
                    return;
                };
                let method = member.property.name.as_str();

                if is_zod_namespace(&member.object) {
                    match method {
                        "object" | "strictObject" | "looseObject" => {
                            if let Some(arg_expr) =
                                call.arguments.first().and_then(Argument::as_expression)
                            {
                                match unwrap_expr(arg_expr) {
                                    Expression::ObjectExpression(obj) => {
                                        let mut ignored_extends = Vec::new();
                                        self.collect_object_properties(
                                            obj,
                                            prefix,
                                            &mut ignored_extends,
                                            fields,
                                        );
                                    }
                                    other => {
                                        if let Some(owner) = extract_shape_owner(other) {
                                            self.expand_local_schema_fields(&owner, prefix, fields);
                                        }
                                    }
                                }
                            }
                        }
                        "array" => {
                            if let Some(arg_expr) =
                                call.arguments.first().and_then(Argument::as_expression)
                            {
                                let elem_prefix = append_array_suffix(prefix);
                                self.collect_nested_field_schema(arg_expr, &elem_prefix, fields);
                            }
                        }
                        "optional" | "nullable" | "readonly" | "nonoptional" | "catch" => {
                            if let Some(arg_expr) =
                                call.arguments.first().and_then(Argument::as_expression)
                            {
                                self.collect_nested_field_schema(arg_expr, prefix, fields);
                            }
                        }
                        "preprocess" => {
                            if let Some(arg_expr) =
                                call.arguments.get(1).and_then(Argument::as_expression)
                            {
                                self.collect_nested_field_schema(arg_expr, prefix, fields);
                            }
                        }
                        "lazy" => {
                            if let Some(arg_expr) =
                                call.arguments.first().and_then(Argument::as_expression)
                                && let Some(returned) = extract_lazy_return_expr(arg_expr)
                            {
                                self.collect_nested_field_schema(returned, prefix, fields);
                            }
                        }
                        "intersection" => {
                            if let Some(left) =
                                call.arguments.first().and_then(Argument::as_expression)
                            {
                                self.collect_nested_field_schema(left, prefix, fields);
                            }
                            if let Some(right) =
                                call.arguments.get(1).and_then(Argument::as_expression)
                            {
                                self.collect_nested_field_schema(right, prefix, fields);
                            }
                        }
                        "union" | "xor" => {
                            if let Some(arg_expr) =
                                call.arguments.first().and_then(Argument::as_expression)
                                && let Expression::ArrayExpression(arr) = unwrap_expr(arg_expr)
                            {
                                for elem in &arr.elements {
                                    if let Some(elem_expr) = elem.as_expression() {
                                        self.collect_nested_field_schema(elem_expr, prefix, fields);
                                    }
                                }
                            }
                        }
                        "discriminatedUnion" => {
                            if let Some(arg_expr) =
                                call.arguments.get(1).and_then(Argument::as_expression)
                                && let Expression::ArrayExpression(arr) = unwrap_expr(arg_expr)
                            {
                                for elem in &arr.elements {
                                    if let Some(elem_expr) = elem.as_expression() {
                                        self.collect_nested_field_schema(elem_expr, prefix, fields);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                match method {
                    "array" => {
                        let elem_prefix = append_array_suffix(prefix);
                        self.collect_nested_field_schema(&member.object, &elem_prefix, fields);
                    }
                    "extend" => {
                        self.collect_nested_field_schema(&member.object, prefix, fields);
                        if let Some(arg_expr) =
                            call.arguments.first().and_then(Argument::as_expression)
                        {
                            match unwrap_expr(arg_expr) {
                                Expression::ObjectExpression(obj) => {
                                    let mut ignored_extends = Vec::new();
                                    self.collect_object_properties(
                                        obj,
                                        prefix,
                                        &mut ignored_extends,
                                        fields,
                                    );
                                }
                                other => {
                                    if let Some(owner) = extract_shape_owner(other) {
                                        self.expand_local_schema_fields(&owner, prefix, fields);
                                    }
                                }
                            }
                        }
                    }
                    "merge" | "and" | "or" => {
                        self.collect_nested_field_schema(&member.object, prefix, fields);
                        if let Some(arg_expr) =
                            call.arguments.first().and_then(Argument::as_expression)
                        {
                            self.collect_nested_field_schema(arg_expr, prefix, fields);
                        }
                    }
                    "pick" | "omit" => {
                        if let Some(evaluated) =
                            self.evaluate_zod_chained_call(&member.object, method, call)
                        {
                            for field in evaluated.fields {
                                push_unique_field(
                                    fields,
                                    TypeFieldDef {
                                        field_path: join_field_path(prefix, &field.field_path),
                                        line: field.line,
                                        snippet: field.snippet,
                                    },
                                );
                            }
                        }
                    }
                    m if is_transparent_zod_modifier(m)
                        || is_array_constraint_modifier(m)
                        || m == "pipe" =>
                    {
                        self.collect_nested_field_schema(&member.object, prefix, fields);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn expand_evaluated_schema(
        &self,
        schema: &EvaluatedSchema,
    ) -> (Vec<TypeFieldDef>, Vec<String>) {
        let mut fields = Vec::new();
        let mut unresolved_extends = Vec::new();
        let mut visited = HashSet::new();

        for parent in &schema.extends {
            self.collect_local_schema_recursive(
                parent,
                &mut visited,
                &mut fields,
                &mut unresolved_extends,
            );
        }

        for field in &schema.fields {
            push_unique_field(&mut fields, field.clone());
        }

        (fields, unresolved_extends)
    }

    fn collect_local_schema_recursive(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        fields: &mut Vec<TypeFieldDef>,
        unresolved_extends: &mut Vec<String>,
    ) {
        if !visited.insert(name.to_string()) {
            return;
        }

        let Some(contract) = self.local_schemas.get(name) else {
            push_unique_extend(unresolved_extends, name.to_string());
            return;
        };

        for parent in &contract.extends {
            self.collect_local_schema_recursive(parent, visited, fields, unresolved_extends);
        }

        for field in &contract.fields {
            push_unique_field(fields, field.clone());
        }
    }

    fn expand_local_schema_fields(
        &self,
        schema_name: &str,
        prefix: &str,
        fields: &mut Vec<TypeFieldDef>,
    ) {
        if !self.local_schemas.contains_key(schema_name) {
            return;
        }
        let mut visited = HashSet::new();
        let mut collected = Vec::new();
        let mut ignored_unresolved = Vec::new();
        self.collect_local_schema_recursive(
            schema_name,
            &mut visited,
            &mut collected,
            &mut ignored_unresolved,
        );
        for field in collected {
            push_unique_field(
                fields,
                TypeFieldDef {
                    field_path: join_field_path(prefix, &field.field_path),
                    line: field.line,
                    snippet: field.snippet,
                },
            );
        }
    }
}

impl<'a> Visit<'a> for ZodCollector<'_> {
    fn visit_variable_declarator(&mut self, decl: &VariableDeclarator<'a>) {
        if let BindingPattern::BindingIdentifier(ident) = &decl.id
            && let Some(init) = &decl.init
            && let Some(mut evaluated) = self.evaluate_schema_expr(init)
        {
            let name = ident.name.to_string();
            evaluated.extends.retain(|parent| parent != &name);
            let line = self.line_index.line_number(decl.span.start);
            let contract = TypeContractDef {
                name: name.clone(),
                file: self.file_path.to_string(),
                line,
                extends: evaluated.extends,
                fields: evaluated.fields,
            };
            self.local_schemas.insert(name, contract.clone());
            self.defs.push(contract);
        }

        walk::walk_variable_declarator(self, decl);
    }

    fn visit_ts_type_alias_declaration(&mut self, decl: &TSTypeAliasDeclaration<'a>) {
        let name = decl.id.name.to_string();
        let line = self.line_index.line_number(decl.span.start);

        let mut extends = Vec::new();
        let mut seen = HashSet::new();
        collect_zod_inferred_refs(&decl.type_annotation, &mut extends, &mut seen);

        // Avoid emitting a self-referential empty alias when value & type share the same name
        // (e.g. `export const User = z.object({...}); export type User = z.infer<typeof User>;`).
        extends.retain(|schema_name| schema_name != &name);

        if !extends.is_empty() {
            self.defs.push(TypeContractDef {
                name,
                file: self.file_path.to_string(),
                line,
                extends,
                fields: Vec::new(),
            });
        }

        walk::walk_ts_type_alias_declaration(self, decl);
    }
}

fn collect_zod_inferred_refs(
    ts_type: &TSType<'_>,
    extends: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match ts_type {
        TSType::TSTypeReference(type_ref) => {
            if is_zod_infer_type_name(&type_ref.type_name) {
                if let Some(args) = &type_ref.type_arguments
                    && let Some(first_arg) = args.params.first()
                    && let Some(schema_name) = extract_typeof_schema_name(first_arg)
                    && seen.insert(schema_name.clone())
                {
                    extends.push(schema_name);
                }
                return;
            }
            if let Some(args) = &type_ref.type_arguments {
                for param in &args.params {
                    collect_zod_inferred_refs(param, extends, seen);
                }
            }
        }
        TSType::TSParenthesizedType(paren) => {
            collect_zod_inferred_refs(&paren.type_annotation, extends, seen);
        }
        TSType::TSArrayType(arr) => {
            collect_zod_inferred_refs(&arr.element_type, extends, seen);
        }
        TSType::TSUnionType(union) => {
            for member in &union.types {
                collect_zod_inferred_refs(member, extends, seen);
            }
        }
        TSType::TSIntersectionType(inter) => {
            for member in &inter.types {
                collect_zod_inferred_refs(member, extends, seen);
            }
        }
        _ => {}
    }
}

fn is_zod_infer_type_name(name: &TSTypeName<'_>) -> bool {
    let TSTypeName::QualifiedName(qn) = name else {
        return false;
    };
    let is_zod_ns = matches!(
        &qn.left,
        TSTypeName::IdentifierReference(id) if matches!(id.name.as_str(), "z" | "zod")
    );
    is_zod_ns
        && matches!(
            qn.right.name.as_str(),
            "infer" | "output" | "input" | "TypeOf"
        )
}

fn extract_typeof_schema_name(ts_type: &TSType<'_>) -> Option<String> {
    match ts_type {
        TSType::TSTypeQuery(query) => match query.expr_name.as_ts_type_name()? {
            TSTypeName::IdentifierReference(id) => Some(id.name.to_string()),
            TSTypeName::QualifiedName(qn) => Some(qn.right.name.to_string()),
            TSTypeName::ThisExpression(_) => None,
        },
        TSType::TSParenthesizedType(paren) => extract_typeof_schema_name(&paren.type_annotation),
        _ => None,
    }
}

fn is_zod_namespace(expr: &Expression<'_>) -> bool {
    matches!(
        unwrap_expr(expr),
        Expression::Identifier(id) if matches!(id.name.as_str(), "z" | "zod")
    )
}

fn extract_schema_ident(expr: &Expression<'_>) -> Option<String> {
    match unwrap_expr(expr) {
        Expression::Identifier(ident) => {
            let name = ident.name.as_str();
            if is_non_zod_schema_ident(name) {
                None
            } else {
                Some(name.to_string())
            }
        }
        Expression::StaticMemberExpression(member) => {
            if member.property.name == "shape" {
                return None;
            }
            let name = member.property.name.as_str();
            if is_non_zod_schema_ident(name) {
                None
            } else {
                Some(name.to_string())
            }
        }
        _ => None,
    }
}

fn extract_shape_owner(expr: &Expression<'_>) -> Option<String> {
    let Expression::StaticMemberExpression(member) = unwrap_expr(expr) else {
        return None;
    };
    if member.property.name != "shape" {
        return None;
    }
    extract_schema_ident(&member.object)
}

fn extract_lazy_return_expr<'a>(expr: &'a Expression<'a>) -> Option<&'a Expression<'a>> {
    match unwrap_expr(expr) {
        Expression::ArrowFunctionExpression(arrow) => match &arrow.body {
            ArrowFunctionBody::FunctionBody(body) => find_first_return_expr(&body.statements),
            _ => arrow.body.as_expression().map(unwrap_expr),
        },
        Expression::FunctionExpression(func) => {
            let body = func.body.as_ref()?;
            find_first_return_expr(&body.statements)
        }
        _ => None,
    }
}

fn find_first_return_expr<'a>(statements: &'a [Statement<'a>]) -> Option<&'a Expression<'a>> {
    for stmt in statements {
        if let Statement::ReturnStatement(ret) = stmt {
            return ret.argument.as_ref().map(unwrap_expr);
        }
    }
    None
}

fn unwrap_expr<'a>(mut expr: &'a Expression<'a>) -> &'a Expression<'a> {
    loop {
        match expr {
            Expression::ParenthesizedExpression(p) => expr = &p.expression,
            Expression::TSAsExpression(ts) => expr = &ts.expression,
            Expression::TSSatisfiesExpression(ts) => expr = &ts.expression,
            Expression::TSTypeAssertion(ts) => expr = &ts.expression,
            Expression::TSNonNullExpression(ts) => expr = &ts.expression,
            _ => return expr,
        }
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

fn has_non_method_or_spread_property(obj: &ObjectExpression<'_>) -> bool {
    obj.properties.iter().any(|prop_kind| match prop_kind {
        ObjectPropertyKind::ObjectProperty(prop) => !prop.method,
        ObjectPropertyKind::SpreadProperty(_) => true,
    })
}

fn is_zod_boolean_mask(obj: &ObjectExpression<'_>) -> bool {
    let mut saw_prop = false;
    for prop_kind in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(prop) = prop_kind else {
            return false;
        };
        if prop.method {
            return false;
        }
        if !matches!(unwrap_expr(&prop.value), Expression::BooleanLiteral(_)) {
            return false;
        }
        saw_prop = true;
    }
    saw_prop
}

fn is_truthy_mask_value(expr: &Expression<'_>) -> bool {
    matches!(unwrap_expr(expr), Expression::BooleanLiteral(lit) if lit.value)
}

fn is_transparent_zod_modifier(method: &str) -> bool {
    matches!(
        method,
        "strict"
            | "passthrough"
            | "strip"
            | "catchall"
            | "partial"
            | "deepPartial"
            | "required"
            | "nullable"
            | "nullish"
            | "optional"
            | "nonoptional"
            | "describe"
            | "meta"
            | "readonly"
            | "default"
            | "catch"
            | "brand"
            | "refine"
            | "superRefine"
            | "transform"
            | "check"
    )
}

fn is_unambiguous_zod_ident_modifier(method: &str) -> bool {
    matches!(
        method,
        "strict"
            | "passthrough"
            | "strip"
            | "catchall"
            | "partial"
            | "deepPartial"
            | "required"
            | "nullable"
            | "nullish"
            | "optional"
            | "nonoptional"
            | "readonly"
            | "brand"
            | "superRefine"
    )
}

fn is_array_constraint_modifier(method: &str) -> bool {
    matches!(method, "min" | "max" | "length" | "nonempty")
}

fn is_non_zod_schema_ident(name: &str) -> bool {
    matches!(
        name,
        "z" | "zod"
            | "Object"
            | "Array"
            | "Promise"
            | "JSON"
            | "Math"
            | "Date"
            | "Number"
            | "String"
            | "Boolean"
            | "RegExp"
            | "Error"
            | "Symbol"
            | "BigInt"
            | "Reflect"
            | "Intl"
            | "URL"
            | "console"
            | "window"
            | "document"
            | "process"
            | "globalThis"
            | "_"
            | "lodash"
            | "dayjs"
            | "moment"
            | "expect"
    )
}

fn is_non_zod_parse_builtin(name: &str) -> bool {
    is_non_zod_schema_ident(name)
        || matches!(
            name,
            "path"
                | "url"
                | "querystring"
                | "qs"
                | "YAML"
                | "yaml"
                | "TOML"
                | "toml"
                | "cookie"
                | "semver"
                | "dotenv"
        )
}

fn join_field_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}.{segment}")
    }
}

fn append_array_suffix(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}[]")
    }
}

fn is_descendant_field(field_path: &str, root_key: &str) -> bool {
    field_path
        .strip_prefix(root_key)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

fn is_field_omitted(field_path: &str, omitted_keys: &HashSet<String>) -> bool {
    let root = field_path.split(['.', '[']).next().unwrap_or(field_path);
    omitted_keys.contains(root)
}

fn push_unique_extend(extends: &mut Vec<String>, parent: String) {
    if !extends.contains(&parent) {
        extends.push(parent);
    }
}

fn push_unique_field(fields: &mut Vec<TypeFieldDef>, field: TypeFieldDef) {
    if !fields
        .iter()
        .any(|existing| existing.field_path == field.field_path && existing.line == field.line)
    {
        fields.push(field);
    }
}

#[cfg(test)]
mod tests {
    use crate::types::resolve_type_fields;

    use super::*;

    #[test]
    fn extracts_basic_zod_object_with_exact_lines_and_snippets() {
        let code = r#"import { z } from "zod";

export const UserSchema = z.object({
  id: z.number(),
  segundoApellido: z.string(),
  "postal-code": z.string(),
});
"#;

        let defs = extract_zod_schemas("src/schemas/user.ts", code).expect("valid TS should parse");
        assert_eq!(defs.len(), 1);

        let user = &defs[0];
        assert_eq!(user.name, "UserSchema");
        assert_eq!(user.file, "src/schemas/user.ts");
        assert_eq!(user.line, 3);
        assert!(user.extends.is_empty());

        let fields: Vec<(&str, usize, &str)> = user
            .fields
            .iter()
            .map(|f| (f.field_path.as_str(), f.line, f.snippet.as_str()))
            .collect();
        assert_eq!(
            fields,
            vec![
                ("id", 4, "id: z.number(),"),
                ("segundoApellido", 5, "segundoApellido: z.string(),"),
                ("postal-code", 6, "\"postal-code\": z.string(),"),
            ]
        );
    }

    #[test]
    fn extracts_nested_zod_objects_and_arrays_of_objects() {
        let code = r#"import { z } from "zod";

export const OrderSchema = z
  .object({
    id: z.number(),
    address: z
      .object({
        city: z.string(),
        geo: z
          .object({
            lat: z.number(),
          })
          .optional(),
      })
      .nullable(),
    items: z
      .array(
        z.object({
          sku: z.string(),
          qty: z.number(),
        }),
      )
      .min(1),
    tags: z
      .object({
        label: z.string(),
      })
      .array(),
  })
  .strict()
  .readonly()
  .describe("Order payload");
"#;

        let defs =
            extract_zod_schemas("src/schemas/order.ts", code).expect("valid TS should parse");
        assert_eq!(defs.len(), 1);

        let order = &defs[0];
        assert_eq!(order.name, "OrderSchema");
        assert_eq!(order.line, 3);

        let paths: Vec<(&str, usize)> = order
            .fields
            .iter()
            .map(|f| (f.field_path.as_str(), f.line))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("id", 5),
                ("address", 6),
                ("address.city", 8),
                ("address.geo", 9),
                ("address.geo.lat", 11),
                ("items", 16),
                ("items[].sku", 19),
                ("items[].qty", 20),
                ("tags", 24),
                ("tags[].label", 26),
            ]
        );
    }

    #[test]
    fn extracts_extend_merge_pick_omit_spread_and_modifiers() {
        let code = r#"import { z } from "zod";
import { ImportedUserSchema } from "./external";

export const UserSchema = z.object({
  id: z.number(),
  segundoApellido: z.string(),
  password: z.string(),
});

export const MetaSchema = z.object({
  createdAt: z.string(),
});

export const AdminSchema = UserSchema.extend({
  role: z.string(),
}).strict();

export const MergedSchema = UserSchema.merge(MetaSchema).passthrough();

export const PickedSchema = UserSchema.pick({
  id: true,
  segundoApellido: true,
});

export const OmittedSchema = UserSchema.omit({
  password: true,
});

export const ExternalOmittedSchema = ImportedUserSchema.omit({
  password: true,
});

export const PartialUserSchema = UserSchema.partial().nullable().optional().required();

export const SpreadSchema = z.object({
  ...UserSchema.shape,
  active: z.boolean(),
});
"#;

        let defs =
            extract_zod_schemas("src/schemas/composed.ts", code).expect("valid TS should parse");
        let by_name: BTreeMap<&str, &TypeContractDef> =
            defs.iter().map(|d| (d.name.as_str(), d)).collect();

        let admin = by_name["AdminSchema"];
        assert_eq!(admin.extends, vec!["UserSchema"]);
        assert_eq!(
            admin
                .fields
                .iter()
                .map(|f| (f.field_path.as_str(), f.line))
                .collect::<Vec<_>>(),
            vec![("role", 15)]
        );

        let merged = by_name["MergedSchema"];
        assert_eq!(merged.extends, vec!["UserSchema", "MetaSchema"]);
        assert!(merged.fields.is_empty());

        let picked = by_name["PickedSchema"];
        assert!(picked.extends.is_empty());
        assert_eq!(
            picked
                .fields
                .iter()
                .map(|f| (f.field_path.as_str(), f.line, f.snippet.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("id", 21, "id: true,"),
                ("segundoApellido", 22, "segundoApellido: true,"),
            ]
        );

        let omitted = by_name["OmittedSchema"];
        assert!(omitted.extends.is_empty());
        assert_eq!(
            omitted
                .fields
                .iter()
                .map(|f| f.field_path.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "segundoApellido"]
        );

        let external_omitted = by_name["ExternalOmittedSchema"];
        assert_eq!(external_omitted.extends, vec!["ImportedUserSchema"]);
        assert!(external_omitted.fields.is_empty());

        let partial = by_name["PartialUserSchema"];
        assert_eq!(partial.extends, vec!["UserSchema"]);
        assert!(partial.fields.is_empty());

        let spread = by_name["SpreadSchema"];
        assert_eq!(spread.extends, vec!["UserSchema"]);
        assert_eq!(
            spread
                .fields
                .iter()
                .map(|f| f.field_path.as_str())
                .collect::<Vec<_>>(),
            vec!["active"]
        );
    }

    #[test]
    fn resolves_z_infer_and_z_output_via_resolve_type_fields() {
        let code = r#"import { z } from "zod";

export const UserSchema = z.object({
  id: z.number(),
  segundoApellido: z.string(),
  address: z.object({
    city: z.string(),
  }),
});

export const AdminSchema = UserSchema.extend({
  role: z.string(),
});

export type User = z.infer<typeof UserSchema>;
export type AdminOutput = z.output<typeof AdminSchema>;

// Same-name value + type idiom must preserve fields without self-overwriting.
export const Profile = z.object({
  bio: z.string(),
});
export type Profile = z.infer<typeof Profile>;
"#;

        let defs = extract_zod_schemas("src/schemas/user.ts", code).expect("valid TS should parse");
        let mut registry = BTreeMap::new();
        for def in defs {
            registry.insert(def.name.clone(), def);
        }

        let user_def = &registry["User"];
        assert_eq!(user_def.extends, vec!["UserSchema"]);
        assert!(user_def.fields.is_empty());

        let user_fields: Vec<String> = resolve_type_fields("User", &registry)
            .into_iter()
            .map(|(_, f)| f.field_path)
            .collect();
        assert_eq!(
            user_fields,
            vec!["address", "address.city", "id", "segundoApellido"]
        );

        let admin_fields: Vec<String> = resolve_type_fields("AdminOutput", &registry)
            .into_iter()
            .map(|(_, f)| f.field_path)
            .collect();
        assert_eq!(
            admin_fields,
            vec!["address", "address.city", "id", "role", "segundoApellido"]
        );

        let profile_fields: Vec<String> = resolve_type_fields("Profile", &registry)
            .into_iter()
            .map(|(_, f)| f.field_path)
            .collect();
        assert_eq!(profile_fields, vec!["bio"]);
    }

    #[test]
    fn extracts_zod_schemas_from_vue_sfc_with_exact_lines() {
        let sfc = "<template>\n  <div>{{ user.segundoApellido }}</div>\n</template>\n\n<script setup lang=\"ts\">\nimport { z } from 'zod';\n\nconst CustomerSchema = z.object({\n  segundoApellido: z.string(),\n});\ntype Customer = z.infer<typeof CustomerSchema>;\n</script>\n";

        let defs =
            extract_zod_schemas("src/components/Customer.vue", sfc).expect("Vue SFC should parse");
        assert_eq!(defs.len(), 2);

        assert_eq!(defs[0].name, "CustomerSchema");
        assert_eq!(defs[0].line, 8);
        assert_eq!(defs[0].fields.len(), 1);
        assert_eq!(defs[0].fields[0].field_path, "segundoApellido");
        assert_eq!(defs[0].fields[0].line, 9);
        assert_eq!(defs[0].fields[0].snippet, "segundoApellido: z.string(),");

        assert_eq!(defs[1].name, "Customer");
        assert_eq!(defs[1].line, 11);
        assert_eq!(defs[1].extends, vec!["CustomerSchema"]);

        let mut registry = BTreeMap::new();
        for def in defs {
            registry.insert(def.name.clone(), def);
        }
        let resolved = resolve_type_fields("Customer", &registry);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].1.field_path, "segundoApellido");
        assert_eq!(resolved[0].1.line, 9);
    }

    #[test]
    fn detects_zod_parse_and_safe_parse_schema_names() {
        let code = r#"
UserSchema.parse(raw);
UserSchema.safeParse(raw);
UserSchema.parseAsync(raw);
UserSchema.safeParseAsync(raw);
UserSchema.array().parse(raw);
z.array(UserSchema).safeParse(raw);
UserSchema.partial().parse(raw);
schemas.OrderSchema.parse(raw);
JSON.parse(rawText);
Date.parse(rawText);
fetch("/users");
"#;

        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, code, SourceType::ts()).parse();
        assert!(parsed.diagnostics.is_empty());

        let results: Vec<Option<String>> = parsed
            .program
            .body
            .iter()
            .filter_map(|stmt| {
                if let Statement::ExpressionStatement(expr_stmt) = stmt
                    && let Expression::CallExpression(call) = &expr_stmt.expression
                {
                    Some(extract_zod_parse_schema_name(call))
                } else {
                    None
                }
            })
            .collect();

        assert_eq!(
            results,
            vec![
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("UserSchema".to_string()),
                Some("OrderSchema".to_string()),
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn ignores_non_zod_utilities_and_primitive_schemas() {
        let code = r#"import { z } from "zod";
const tags = z.array(z.string());
const picked = _.pick(obj, ["id"]);
const omitted = _.omit(obj, ["password"]);
const extended = dayjs.extend(utc);
const merged = _.merge(a, b);
const caught = promise.catch(handleErr);
"#;

        let defs = extract_zod_schemas("src/utils.ts", code).expect("valid TS should parse");
        assert!(defs.is_empty());
    }
}
