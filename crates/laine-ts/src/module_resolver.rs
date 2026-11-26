use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};

use oxc_ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, Expression,
    ImportDeclarationSpecifier, ImportOrExportKind, ModuleExportName, Program, Statement,
};

const PROBE_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".vue", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs",
];

const BARREL_INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.mts",
    "index.js",
    "index.jsx",
    "index.mjs",
];

const FALLBACK_ALIAS_ROOTS: &[&str] = &["app", "src", ""];

const MAX_EXTENDS_DEPTH: usize = 32;
const MAX_EXPORT_DEPTH: usize = 32;
const MAX_JSON_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ImportedSymbol {
    pub specifier: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub(crate) struct ModuleSymbols {
    pub imports: BTreeMap<String, ImportedSymbol>,
    exports: BTreeMap<String, String>,
    reexports: BTreeMap<String, ImportedSymbol>,
    stars: Vec<String>,
}

impl ModuleSymbols {
    /// Records top-level imports and exports, including aliases and barrel forwards.
    pub(crate) fn collect(&mut self, program: &Program<'_>, include_type_only: bool) {
        for stmt in &program.body {
            match stmt {
                Statement::ImportDeclaration(decl)
                    if include_type_only || decl.import_kind != ImportOrExportKind::Type =>
                {
                    let Some(source) = decl.source.value.as_str() else {
                        continue;
                    };
                    for spec in decl.specifiers.iter().flatten() {
                        let (local, name) = match spec {
                            ImportDeclarationSpecifier::ImportSpecifier(named)
                                if include_type_only
                                    || named.import_kind != ImportOrExportKind::Type =>
                            {
                                (&named.local, export_name(&named.imported))
                            }
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(default) => {
                                (&default.local, Some("default"))
                            }
                            _ => continue,
                        };
                        if let Some(name) = name {
                            self.imports.insert(
                                local.name.to_string(),
                                ImportedSymbol {
                                    specifier: source.into(),
                                    name: name.into(),
                                },
                            );
                        }
                    }
                }
                Statement::ExportFromDeclaration(decl)
                    if include_type_only || decl.export_kind != ImportOrExportKind::Type =>
                {
                    for spec in &decl.specifiers {
                        if !include_type_only && spec.export_kind == ImportOrExportKind::Type {
                            continue;
                        }
                        let (Some(local), Some(exported)) =
                            (export_name(&spec.local), export_name(&spec.exported))
                        else {
                            continue;
                        };
                        if let Some(specifier) = decl.source.value.as_str() {
                            self.reexports.insert(
                                exported.into(),
                                ImportedSymbol {
                                    specifier: specifier.into(),
                                    name: local.into(),
                                },
                            );
                        }
                    }
                }
                Statement::ExportNamedDeclaration(decl)
                    if include_type_only || decl.export_kind != ImportOrExportKind::Type =>
                {
                    for spec in &decl.specifiers {
                        if (include_type_only || spec.export_kind != ImportOrExportKind::Type)
                            && let (Some(local), Some(exported)) =
                                (export_name(&spec.local), export_name(&spec.exported))
                        {
                            self.exports.insert(exported.into(), local.into());
                        }
                    }
                }
                Statement::ExportAllDeclaration(decl)
                    if include_type_only || decl.export_kind != ImportOrExportKind::Type =>
                {
                    if decl.exported.is_none()
                        && let Some(source) = decl.source.value.as_str()
                    {
                        self.stars.push(source.into());
                    } else if let Some(name) = decl.exported.as_ref().and_then(export_name) {
                        self.exports.insert(name.into(), name.into());
                    }
                }
                Statement::ExportDeclaration(decl) => match &decl.declaration {
                    Declaration::VariableDeclaration(vars) => {
                        for var in &vars.declarations {
                            if let BindingPattern::BindingIdentifier(id) = &var.id {
                                self.exports
                                    .insert(id.name.to_string(), id.name.to_string());
                            }
                        }
                    }
                    Declaration::TSInterfaceDeclaration(item) if include_type_only => {
                        self.exports
                            .insert(item.id.name.to_string(), item.id.name.to_string());
                    }
                    Declaration::TSTypeAliasDeclaration(item) if include_type_only => {
                        self.exports
                            .insert(item.id.name.to_string(), item.id.name.to_string());
                    }
                    Declaration::FunctionDeclaration(item) => {
                        if let Some(id) = &item.id {
                            self.exports
                                .insert(id.name.to_string(), id.name.to_string());
                        }
                    }
                    Declaration::ClassDeclaration(item) => {
                        if let Some(id) = &item.id {
                            self.exports
                                .insert(id.name.to_string(), id.name.to_string());
                        }
                    }
                    _ => {}
                },
                Statement::ExportDefaultDeclaration(decl) => {
                    let name = match &decl.declaration {
                        ExportDefaultDeclarationKind::TSInterfaceDeclaration(item)
                            if include_type_only =>
                        {
                            item.id.name.as_str()
                        }
                        other => {
                            match other.as_expression().map(Expression::get_inner_expression) {
                                Some(Expression::Identifier(id)) => id.name.as_str(),
                                _ => "default",
                            }
                        }
                    };
                    self.exports.insert("default".into(), name.into());
                }
                _ => {}
            }
        }
    }
}

/// Returns the spelling used to import or export a module symbol.
fn export_name<'a>(name: &'a ModuleExportName<'_>) -> Option<&'a str> {
    match name {
        ModuleExportName::IdentifierName(id) => Some(id.name.as_str()),
        ModuleExportName::IdentifierReference(id) => Some(id.name.as_str()),
        ModuleExportName::StringLiteral(value) => value.value.as_str(),
    }
}

/// Resolves a named or default import to the actual declaring module and local symbol.
pub(crate) fn resolve_imported_symbol(
    file: &str,
    import: &ImportedSymbol,
    modules: &BTreeMap<String, ModuleSymbols>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<(String, String)> {
    let target = resolve_import(file, &import.specifier)?;
    resolve_export(
        &target,
        &import.name,
        modules,
        resolve_import,
        &mut HashSet::new(),
    )
    .ok()
    .flatten()
}

/// Follows explicit exports and accepts star exports only when their origin is unique.
fn resolve_export(
    file: &str,
    name: &str,
    modules: &BTreeMap<String, ModuleSymbols>,
    resolve_import: &dyn Fn(&str, &str) -> Option<String>,
    active: &mut HashSet<(String, String)>,
) -> Result<Option<(String, String)>, ()> {
    let Some(module) = modules.get(file) else {
        return Ok(None);
    };
    let key = (file.to_owned(), name.to_owned());
    if active.len() >= MAX_EXPORT_DEPTH {
        return Err(());
    }
    if !active.insert(key.clone()) {
        return Ok(None);
    }
    let follow = |import: &ImportedSymbol, active: &mut HashSet<_>| {
        let Some(target) = resolve_import(file, &import.specifier) else {
            return Ok(None);
        };
        resolve_export(&target, &import.name, modules, resolve_import, active)
    };
    let result = if let Some(import) = module.reexports.get(name) {
        follow(import, active)
    } else if let Some(local) = module.exports.get(name) {
        match module.imports.get(local) {
            Some(import) => follow(import, active),
            None => Ok(Some((file.to_owned(), local.clone()))),
        }
    } else if name == "default" {
        Ok(None)
    } else {
        module.stars.iter().try_fold(None, |found, specifier| {
            if let Some(target) = resolve_import(file, specifier)
                && let Some(origin) =
                    resolve_export(&target, name, modules, resolve_import, active)?
            {
                if found.as_ref().is_some_and(|previous| previous != &origin) {
                    return Err(());
                }
                return Ok(Some(origin));
            }
            Ok(found)
        })
    };
    active.remove(&key);
    result
}

/// Fast, deterministic, zero-disk-I/O TypeScript/JavaScript/Vue module resolver
/// operating purely over tracked POSIX file paths and config files from a Git snapshot.
#[derive(Debug, Clone)]
pub struct ModuleResolver {
    files: HashSet<String>,
    base_url: Option<String>,
    /// (pattern, replacement targets) from `tsconfig.json` `compilerOptions.paths`,
    /// ordered deterministically by longest static prefix first.
    path_aliases: Vec<(String, Vec<String>)>,
}

impl ModuleResolver {
    /// Builds a resolver from tracked POSIX file paths in a repo snapshot and a map of
    /// config files (e.g. `"tsconfig.json"` / `"jsconfig.json"` / extended base configs -> raw JSONC).
    pub fn from_git_snapshot(
        tracked_files: &[String],
        tsconfig_sources: &BTreeMap<String, String>,
    ) -> Self {
        let mut files = HashSet::with_capacity(tracked_files.len());
        for path in tracked_files {
            if let Some(normalized) = normalize_posix_path(path)
                && !normalized.is_empty()
            {
                files.insert(normalized);
            }
        }

        let normalized_configs: BTreeMap<String, &str> = tsconfig_sources
            .iter()
            .filter_map(|(path, content)| {
                let normalized = normalize_posix_path(path)?;
                if normalized.is_empty() {
                    None
                } else {
                    Some((normalized, content.as_str()))
                }
            })
            .collect();

        let root_config_key = if normalized_configs.contains_key("tsconfig.json") {
            Some("tsconfig.json")
        } else if normalized_configs.contains_key("jsconfig.json") {
            Some("jsconfig.json")
        } else {
            None
        };

        let (base_url, path_aliases) = match root_config_key {
            Some(root_key) => build_tsconfig_rules(root_key, &normalized_configs),
            None => (None, Vec::new()),
        };

        Self {
            files,
            base_url,
            path_aliases,
        }
    }

    /// Resolves an import specifier (relative `./foo`, parent `../bar`, tsconfig alias `@/services/user`,
    /// Nuxt/Vite alias `~/services/user`, or `baseUrl` import) from `importer_file` into a tracked POSIX path.
    /// Returns `None` for bare npm package imports like `"react"`, `"axios"`, `"vue"`, `"zod"`.
    pub fn resolve_import(&self, importer_file: &str, specifier: &str) -> Option<String> {
        let raw_specifier = strip_query_and_fragment(specifier.trim());
        if raw_specifier.is_empty() || is_non_file_specifier(raw_specifier) {
            return None;
        }

        let normalized_spec = if raw_specifier.contains('\\') {
            raw_specifier.replace('\\', "/")
        } else {
            raw_specifier.to_string()
        };
        let spec = normalized_spec.as_str();
        let dir_only = spec.ends_with('/') || spec == "." || spec == "..";

        // 1. Relative imports (`./`, `../`, `.`, `..`).
        if is_relative_specifier(spec) {
            let normalized_importer = normalize_posix_path(importer_file)?;
            let importer_dir = parent_dir(&normalized_importer);
            let candidate = join_and_normalize(importer_dir, spec)?;
            return self.probe_candidate(&candidate, dir_only);
        }

        // 2. Explicit `tsconfig.json` / `jsconfig.json` `compilerOptions.paths` aliases.
        let (matched_tsconfig_alias, resolved_alias) = self.resolve_path_aliases(spec, dir_only);
        if resolved_alias.is_some() {
            return resolved_alias;
        }

        // 3. Smart fallback aliases (`@/` and `~/`) when no explicit tsconfig entry matched.
        if !matched_tsconfig_alias
            && let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/"))
        {
            for root in FALLBACK_ALIAS_ROOTS {
                if let Some(candidate) = join_and_normalize(root, rest)
                    && let Some(found) = self.probe_candidate(&candidate, dir_only)
                {
                    return Some(found);
                }
            }
        }

        // 4. `compilerOptions.baseUrl` lookup.
        if let Some(base_url) = &self.base_url
            && let Some(candidate) = join_and_normalize(base_url, spec)
            && let Some(found) = self.probe_candidate(&candidate, dir_only)
        {
            return Some(found);
        }

        // 5. Bare npm package specifier (or unresolved import).
        None
    }

    fn resolve_path_aliases(&self, specifier: &str, dir_only: bool) -> (bool, Option<String>) {
        let mut matched_any = false;
        for (pattern, targets) in &self.path_aliases {
            let Some(captured) = match_path_pattern(pattern, specifier) else {
                continue;
            };
            matched_any = true;
            for target in targets {
                let substituted = match captured {
                    Some(wildcard) => target.replacen('*', wildcard, 1),
                    None => target.clone(),
                };
                if let Some(candidate) = normalize_posix_path(&substituted)
                    && let Some(found) = self.probe_candidate(&candidate, dir_only)
                {
                    return (true, Some(found));
                }
            }
        }
        (matched_any, None)
    }

    /// Probes a normalized repo-relative candidate path in TypeScript/ESM resolution order:
    /// 1. Exact match in `self.files`.
    /// 2. TypeScript ESM extension remapping (`.js`/`.mjs`/`.cjs`/`.jsx` -> `.ts`/`.tsx`/`.mts`/`.cts`).
    /// 3. Extensionless append (`.ts`, `.tsx`, `.vue`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs`).
    /// 4. Directory barrel index files (`/index.ts`, `/index.tsx`, `/index.mts`, `/index.js`, `/index.jsx`, `/index.mjs`).
    fn probe_candidate(&self, candidate: &str, dir_only: bool) -> Option<String> {
        if !candidate.is_empty() && !dir_only {
            // 1. Exact file match.
            if self.files.contains(candidate) {
                return Some(candidate.to_string());
            }

            // 2. TypeScript NodeNext/ESM extension remapping.
            if let Some((stem, replacements)) = esm_extension_remaps(candidate) {
                for ext in replacements {
                    let remapped = format!("{stem}{ext}");
                    if self.files.contains(&remapped) {
                        return Some(remapped);
                    }
                }
            }

            // 3. Append supported extensions in order.
            for ext in PROBE_EXTENSIONS {
                let with_ext = format!("{candidate}{ext}");
                if self.files.contains(&with_ext) {
                    return Some(with_ext);
                }
            }
        }

        // 4. Directory barrel index files in order.
        for index_file in BARREL_INDEX_FILES {
            let barrel = if candidate.is_empty() {
                (*index_file).to_string()
            } else {
                format!("{candidate}/{index_file}")
            };
            if self.files.contains(&barrel) {
                return Some(barrel);
            }
        }

        None
    }
}

#[derive(Debug, Clone)]
struct RawPathTarget {
    target: String,
    declaring_dir: String,
    anchored_to_config_dir: bool,
}

#[derive(Debug, Default)]
struct MergedTsConfig {
    base_url: Option<String>,
    paths: BTreeMap<String, Vec<RawPathTarget>>,
}

fn build_tsconfig_rules(
    root_key: &str,
    configs: &BTreeMap<String, &str>,
) -> (Option<String>, Vec<(String, Vec<String>)>) {
    let root_config_dir = parent_dir(root_key);
    let mut active = HashSet::new();
    let merged = load_tsconfig_recursive(root_key, root_config_dir, configs, &mut active, 0);

    let explicit_base_url = merged.base_url;
    let mut alias_entries: Vec<(String, Vec<String>)> = Vec::new();

    for (pattern, raw_targets) in merged.paths {
        if !is_valid_path_pattern(&pattern) {
            continue;
        }
        let mut resolved_targets = Vec::new();
        for raw in raw_targets {
            let candidate = if raw.anchored_to_config_dir {
                normalize_posix_path(&raw.target)
            } else if let Some(base) = explicit_base_url.as_deref() {
                join_and_normalize(base, &raw.target)
            } else {
                join_and_normalize(&raw.declaring_dir, &raw.target)
            };

            if let Some(normalized) = candidate
                && !normalized.is_empty()
            {
                resolved_targets.push(normalized);
            }
        }

        if !resolved_targets.is_empty() {
            alias_entries.push((pattern, resolved_targets));
        }
    }

    // Order deterministically by longest static prefix first, then exact before wildcard,
    // then longest static suffix, then lexicographical pattern order.
    alias_entries.sort_by(|(a, _), (b, _)| pattern_sort_key(a).cmp(&pattern_sort_key(b)));

    let effective_base_url = match explicit_base_url {
        Some(base) => Some(base),
        None if !alias_entries.is_empty() => Some(".".to_string()),
        None => None,
    };

    (effective_base_url, alias_entries)
}

fn load_tsconfig_recursive(
    config_path: &str,
    root_config_dir: &str,
    configs: &BTreeMap<String, &str>,
    active: &mut HashSet<String>,
    depth: usize,
) -> MergedTsConfig {
    if depth >= MAX_EXTENDS_DEPTH || !active.insert(config_path.to_string()) {
        return MergedTsConfig::default();
    }

    let mut result = MergedTsConfig::default();
    let Some(raw_source) = configs.get(config_path).copied() else {
        active.remove(config_path);
        return result;
    };

    let Some(JsonValue::Object(root_obj)) = parse_jsonc(raw_source) else {
        active.remove(config_path);
        return result;
    };

    let current_dir = parent_dir(config_path);

    // 1. Follow `extends` (single string or TS 5.0+ array of strings).
    if let Some(extends_val) = root_obj.get("extends") {
        let extends_specs: Vec<&str> = match extends_val {
            JsonValue::String(s) => vec![s.as_str()],
            JsonValue::Array(items) => items
                .iter()
                .filter_map(|item| match item {
                    JsonValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        for spec in extends_specs {
            if let Some(parent_key) = resolve_extends_key(current_dir, spec, configs) {
                let parent_cfg = load_tsconfig_recursive(
                    &parent_key,
                    root_config_dir,
                    configs,
                    active,
                    depth + 1,
                );
                if let Some(parent_base) = parent_cfg.base_url {
                    result.base_url = Some(parent_base);
                }
                for (pattern, targets) in parent_cfg.paths {
                    result.paths.insert(pattern, targets);
                }
            }
        }
    }

    // 2. Apply this config's own `compilerOptions` (child overrides parent).
    if let Some(JsonValue::Object(compiler_opts)) = root_obj.get("compilerOptions") {
        if let Some(JsonValue::String(raw_base)) = compiler_opts.get("baseUrl") {
            let (expanded, anchored) = expand_config_dir(raw_base.trim(), root_config_dir);
            let normalized = if anchored {
                normalize_posix_path(&expanded)
            } else {
                join_and_normalize(current_dir, &expanded)
            };
            if let Some(base) = normalized {
                result.base_url = Some(if base.is_empty() {
                    ".".to_string()
                } else {
                    base
                });
            }
        }

        if let Some(JsonValue::Object(paths_obj)) = compiler_opts.get("paths") {
            for (pattern, targets_val) in paths_obj {
                let JsonValue::Array(items) = targets_val else {
                    continue;
                };
                let mut targets = Vec::new();
                for item in items {
                    if let JsonValue::String(raw_target) = item {
                        let trimmed = raw_target.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let (expanded, anchored) = expand_config_dir(trimmed, root_config_dir);
                        targets.push(RawPathTarget {
                            target: expanded,
                            declaring_dir: current_dir.to_string(),
                            anchored_to_config_dir: anchored,
                        });
                    }
                }
                result.paths.insert(pattern.clone(), targets);
            }
        }
    }

    active.remove(config_path);
    result
}

fn resolve_extends_key(
    current_dir: &str,
    extends_spec: &str,
    configs: &BTreeMap<String, &str>,
) -> Option<String> {
    let trimmed = extends_spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    let base_candidate = if is_relative_specifier(trimmed) {
        join_and_normalize(current_dir, trimmed)?
    } else {
        normalize_posix_path(trimmed)?
    };

    if base_candidate.is_empty() {
        return None;
    }

    if configs.contains_key(&base_candidate) {
        return Some(base_candidate);
    }

    if !base_candidate.ends_with(".json") {
        let with_json = format!("{base_candidate}.json");
        if configs.contains_key(&with_json) {
            return Some(with_json);
        }
    }

    let dir_tsconfig = format!("{base_candidate}/tsconfig.json");
    if configs.contains_key(&dir_tsconfig) {
        return Some(dir_tsconfig);
    }

    None
}

fn expand_config_dir(value: &str, root_config_dir: &str) -> (String, bool) {
    if !value.contains("${configDir}") {
        return (value.to_string(), false);
    }

    let expanded = if root_config_dir.is_empty() {
        if let Some(rest) = value.strip_prefix("${configDir}/") {
            rest.to_string()
        } else if value == "${configDir}" {
            ".".to_string()
        } else {
            value
                .replace("${configDir}/", "")
                .replace("${configDir}", ".")
        }
    } else {
        value.replace("${configDir}", root_config_dir)
    };

    (expanded, true)
}

fn is_valid_path_pattern(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.bytes().filter(|&b| b == b'*').count() <= 1
}

fn pattern_sort_key(pattern: &str) -> (Reverse<usize>, bool, Reverse<usize>, &str) {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => (Reverse(prefix.len()), true, Reverse(suffix.len()), pattern),
        None => (Reverse(pattern.len()), false, Reverse(0), pattern),
    }
}

/// Matches a `tsconfig.json` `paths` pattern against an import specifier.
/// - Returns `None` if it does not match.
/// - Returns `Some(None)` for an exact match (pattern has no `*`).
/// - Returns `Some(Some(captured))` for a single-wildcard `*` match.
fn match_path_pattern<'a>(pattern: &str, specifier: &'a str) -> Option<Option<&'a str>> {
    match pattern.split_once('*') {
        None => {
            if specifier == pattern {
                Some(None)
            } else {
                None
            }
        }
        Some((prefix, suffix)) => {
            if specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
            {
                let captured = &specifier[prefix.len()..specifier.len() - suffix.len()];
                Some(Some(captured))
            } else {
                None
            }
        }
    }
}

fn esm_extension_remaps(candidate: &str) -> Option<(&str, &'static [&'static str])> {
    if let Some(stem) = candidate.strip_suffix(".js") {
        Some((stem, &[".ts", ".tsx", ".mts", ".cts"]))
    } else if let Some(stem) = candidate.strip_suffix(".jsx") {
        Some((stem, &[".tsx", ".ts", ".mts", ".cts"]))
    } else if let Some(stem) = candidate.strip_suffix(".mjs") {
        Some((stem, &[".mts", ".ts", ".tsx", ".cts"]))
    } else if let Some(stem) = candidate.strip_suffix(".cjs") {
        Some((stem, &[".cts", ".ts", ".tsx", ".mts"]))
    } else {
        None
    }
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

fn is_non_file_specifier(specifier: &str) -> bool {
    specifier.starts_with("http://")
        || specifier.starts_with("https://")
        || specifier.starts_with("node:")
        || specifier.starts_with("data:")
}

fn strip_query_and_fragment(specifier: &str) -> &str {
    specifier.split(['?', '#']).next().unwrap_or(specifier)
}

fn normalize_posix_path(raw: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in raw.split(['/', '\\']) {
        match segment {
            "" | "." => continue,
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

fn join_and_normalize(base: &str, rel: &str) -> Option<String> {
    if base.is_empty() || base == "." {
        normalize_posix_path(rel)
    } else {
        normalize_posix_path(&format!("{base}/{rel}"))
    }
}

fn parent_dir(normalized_path: &str) -> &str {
    match normalized_path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

// ---------------------------------------------------------------------------
// Minimal pure-Rust JSONC parser (supports `//` comments, `/* */` comments,
// trailing commas, UTF-8 BOM, and standard JSON string escapes).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonValue {
    Object(BTreeMap<String, JsonValue>),
    Array(Vec<JsonValue>),
    String(String),
    Bool(bool),
    Number,
    Null,
}

fn parse_jsonc(input: &str) -> Option<JsonValue> {
    let stripped = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut parser = JsoncParser {
        src: stripped,
        pos: 0,
        depth: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws_and_comments();
    if parser.pos == parser.src.len() {
        Some(value)
    } else {
        None
    }
}

struct JsoncParser<'a> {
    src: &'a str,
    pos: usize,
    depth: usize,
}

impl<'a> JsoncParser<'a> {
    fn peek_byte(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn remaining(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn skip_ws_and_comments(&mut self) {
        let bytes = self.src.as_bytes();
        loop {
            while self.pos < bytes.len() && matches!(bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.pos += 1;
            }

            if self.pos + 1 < bytes.len() && bytes[self.pos] == b'/' {
                match bytes[self.pos + 1] {
                    b'/' => {
                        self.pos += 2;
                        while self.pos < bytes.len() && bytes[self.pos] != b'\n' {
                            self.pos += 1;
                        }
                        continue;
                    }
                    b'*' => {
                        self.pos += 2;
                        if let Some(rel_end) = self.remaining().find("*/") {
                            self.pos += rel_end + 2;
                        } else {
                            self.pos = bytes.len();
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            break;
        }
    }

    fn parse_value(&mut self) -> Option<JsonValue> {
        if self.depth >= MAX_JSON_DEPTH {
            return None;
        }
        self.skip_ws_and_comments();
        let byte = self.peek_byte()?;
        match byte {
            b'{' => {
                self.depth += 1;
                let obj = self.parse_object();
                self.depth -= 1;
                obj.map(JsonValue::Object)
            }
            b'[' => {
                self.depth += 1;
                let arr = self.parse_array();
                self.depth -= 1;
                arr.map(JsonValue::Array)
            }
            b'"' => self.parse_string().map(JsonValue::String),
            b't' => self.consume_literal("true").map(|()| JsonValue::Bool(true)),
            b'f' => self
                .consume_literal("false")
                .map(|()| JsonValue::Bool(false)),
            b'n' => self.consume_literal("null").map(|()| JsonValue::Null),
            b'-' | b'0'..=b'9' => self.parse_number().map(|()| JsonValue::Number),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Option<BTreeMap<String, JsonValue>> {
        if self.peek_byte() != Some(b'{') {
            return None;
        }
        self.pos += 1;
        let mut map = BTreeMap::new();

        self.skip_ws_and_comments();
        if self.peek_byte() == Some(b'}') {
            self.pos += 1;
            return Some(map);
        }

        loop {
            self.skip_ws_and_comments();
            let key = self.parse_string()?;
            self.skip_ws_and_comments();
            if self.peek_byte() != Some(b':') {
                return None;
            }
            self.pos += 1;
            let value = self.parse_value()?;
            map.insert(key, value);

            self.skip_ws_and_comments();
            match self.peek_byte() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws_and_comments();
                    if self.peek_byte() == Some(b'}') {
                        self.pos += 1;
                        return Some(map);
                    }
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Some(map);
                }
                _ => return None,
            }
        }
    }

    fn parse_array(&mut self) -> Option<Vec<JsonValue>> {
        if self.peek_byte() != Some(b'[') {
            return None;
        }
        self.pos += 1;
        let mut items = Vec::new();

        self.skip_ws_and_comments();
        if self.peek_byte() == Some(b']') {
            self.pos += 1;
            return Some(items);
        }

        loop {
            let value = self.parse_value()?;
            items.push(value);

            self.skip_ws_and_comments();
            match self.peek_byte() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws_and_comments();
                    if self.peek_byte() == Some(b']') {
                        self.pos += 1;
                        return Some(items);
                    }
                }
                Some(b']') => {
                    self.pos += 1;
                    return Some(items);
                }
                _ => return None,
            }
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.peek_byte() != Some(b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();

        while let Some(ch) = self.remaining().chars().next() {
            self.pos += ch.len_utf8();
            match ch {
                '"' => return Some(out),
                '\\' => {
                    let esc = self.remaining().chars().next()?;
                    self.pos += esc.len_utf8();
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000C}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let code_unit = self.parse_hex4()?;
                            if (0xD800..=0xDBFF).contains(&code_unit) {
                                // High surrogate: expect `\uXXXX` low surrogate.
                                if !self.remaining().starts_with("\\u") {
                                    return None;
                                }
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return None;
                                }
                                let combined = 0x10000
                                    + (((code_unit - 0xD800) as u32) << 10)
                                    + ((low - 0xDC00) as u32);
                                out.push(char::from_u32(combined)?);
                            } else {
                                out.push(char::from_u32(code_unit as u32)?);
                            }
                        }
                        _ => return None,
                    }
                }
                c if (c as u32) < 0x20 => return None,
                c => out.push(c),
            }
        }

        None
    }

    fn parse_hex4(&mut self) -> Option<u16> {
        let slice = self.remaining().get(..4)?;
        let value = u16::from_str_radix(slice, 16).ok()?;
        self.pos += 4;
        Some(value)
    }

    fn consume_literal(&mut self, literal: &str) -> Option<()> {
        if self.remaining().starts_with(literal) {
            self.pos += literal.len();
            Some(())
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<()> {
        let bytes = self.src.as_bytes();
        if self.peek_byte() == Some(b'-') {
            self.pos += 1;
        }

        match self.peek_byte()? {
            b'0' => {
                self.pos += 1;
            }
            b'1'..=b'9' => {
                self.pos += 1;
                while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                    self.pos += 1;
                }
            }
            _ => return None,
        }

        if self.peek_byte() == Some(b'.') {
            self.pos += 1;
            if !self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                return None;
            }
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }

        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek_byte(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                return None;
            }
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }

        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    fn configs(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn resolves_relative_ts_tsx_vue_and_barrel_imports() {
        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "src/components/UserCard.vue",
                "src/components/Avatar.tsx",
                "src/services/user.ts",
                "src/services/index.ts",
                "src/utils/format.mts",
                "src/legacy/helper.js",
                "src/mixed/index.tsx",
                "index.ts",
            ]),
            &BTreeMap::new(),
        );

        assert_eq!(
            resolver.resolve_import("src/components/UserCard.vue", "../services/user"),
            Some("src/services/user.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/services/user.ts", "../components/UserCard.vue"),
            Some("src/components/UserCard.vue".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/services/user.ts", "../components/UserCard"),
            Some("src/components/UserCard.vue".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/components/UserCard.vue", "./Avatar"),
            Some("src/components/Avatar.tsx".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/components/UserCard.vue", "../services"),
            Some("src/services/index.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/components/UserCard.vue", "../utils/format"),
            Some("src/utils/format.mts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/components/UserCard.vue", "../legacy/helper"),
            Some("src/legacy/helper.js".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/services/user.ts", "../mixed"),
            Some("src/mixed/index.tsx".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/services/user.ts", "."),
            Some("src/services/index.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/services/user.ts", "../.."),
            Some("index.ts".to_string())
        );
    }

    #[test]
    fn resolves_extension_precedence_and_explicit_directory_slash() {
        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "src/dup.ts",
                "src/dup.tsx",
                "src/dup.js",
                "src/mod.ts",
                "src/mod/index.ts",
            ]),
            &BTreeMap::new(),
        );

        assert_eq!(
            resolver.resolve_import("src/main.ts", "./dup"),
            Some("src/dup.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/main.ts", "./dup.js"),
            Some("src/dup.js".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/main.ts", "./mod"),
            Some("src/mod.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/main.ts", "./mod/"),
            Some("src/mod/index.ts".to_string())
        );
    }

    #[test]
    fn remaps_esm_js_jsx_mjs_cjs_extensions_to_typescript() {
        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "src/services/user.ts",
                "src/components/Button.tsx",
                "src/components/Card.ts",
                "src/components/Card.tsx",
                "src/esm/util.mts",
                "src/cjs/legacy.cts",
                "src/barrel/index.ts",
            ]),
            &BTreeMap::new(),
        );

        assert_eq!(
            resolver.resolve_import("src/app.ts", "./services/user.js"),
            Some("src/services/user.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/app.ts", "./components/Button.js"),
            Some("src/components/Button.tsx".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/app.ts", "./components/Card.jsx"),
            Some("src/components/Card.tsx".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/app.ts", "./esm/util.mjs"),
            Some("src/esm/util.mts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/app.ts", "./cjs/legacy.cjs"),
            Some("src/cjs/legacy.cts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/app.ts", "./barrel/index.js"),
            Some("src/barrel/index.ts".to_string())
        );
    }

    #[test]
    fn parses_jsonc_tsconfig_with_comments_trailing_commas_paths_and_config_dir() {
        let tsconfig = "\u{feff}{
            // Schema URL containing double slashes inside a string!
            \"$schema\": \"https://json.schemastore.org/tsconfig\",
            /* Multi-line comment
               before compilerOptions */
            \"compilerOptions\": {
                \"strict\": true,
                \"target\": \"ES2022\",
                \"maxNodeModuleJsDepth\": 2,
                \"baseUrl\": \".\",
                \"paths\": {
                    // Wildcard strings contain /* which must NOT start a comment!
                    \"@/*\": [\"src/*\",],
                    \"@components/*\": [\"${configDir}/src/components/*\"],
                    \"~/*\": [\"app/*\", \"src/*\",],
                    \"@api\": [\"src/api/client.ts\"],
                },
            },
        }";

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "src/services/user.ts",
                "src/components/UserCard.vue",
                "src/api/client.ts",
                "app/composables/useAuth.ts",
                "src/shared/date.ts",
                "lib/rootUtil.ts",
            ]),
            &configs(&[("tsconfig.json", tsconfig)]),
        );

        // `@/*` wildcard alias.
        assert_eq!(
            resolver.resolve_import("src/main.ts", "@/services/user"),
            Some("src/services/user.ts".to_string())
        );
        // More specific `@components/*` with `${configDir}/` prefix.
        assert_eq!(
            resolver.resolve_import("src/main.ts", "@components/UserCard"),
            Some("src/components/UserCard.vue".to_string())
        );
        // Multi-target `~/*`: first target (`app/*`) hits when present, second (`src/*`) hits as fallback.
        assert_eq!(
            resolver.resolve_import("src/main.ts", "~/composables/useAuth"),
            Some("app/composables/useAuth.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/main.ts", "~/shared/date"),
            Some("src/shared/date.ts".to_string())
        );
        // Exact alias `@api`.
        assert_eq!(
            resolver.resolve_import("src/main.ts", "@api"),
            Some("src/api/client.ts".to_string())
        );
        assert_eq!(resolver.resolve_import("src/main.ts", "@api/extra"), None);
        // `baseUrl: "."` import.
        assert_eq!(
            resolver.resolve_import("src/main.ts", "lib/rootUtil"),
            Some("lib/rootUtil.ts".to_string())
        );
    }

    #[test]
    fn tsconfig_paths_without_base_url_defaults_to_repo_root() {
        let tsconfig = r#"{
            "compilerOptions": {
                "paths": {
                    "@/*": ["./src/*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/services/order.ts"]),
            &configs(&[("tsconfig.json", tsconfig)]),
        );

        assert_eq!(
            resolver.resolve_import("src/app.ts", "@/services/order"),
            Some("src/services/order.ts".to_string())
        );
    }

    #[test]
    fn resolves_tsconfig_extends_chain_and_child_overrides() {
        let base = r#"{
            // Shared workspace base config
            "compilerOptions": {
                "baseUrl": "src",
                "paths": {
                    "@/*": ["./*"],
                    "@shared/*": ["../packages/shared/*"],
                }
            }
        }"#;

        let child = r#"{
            "extends": "./tsconfig.base",
            "compilerOptions": {
                "paths": {
                    // Child overrides `@/*` while keeping inherited `@shared/*`
                    "@/*": ["../app/*"],
                    "@api": ["api/client.ts"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "app/services/user.ts",
                "src/services/user.ts",
                "src/api/client.ts",
                "src/models/account.ts",
                "packages/shared/types.ts",
            ]),
            &configs(&[("tsconfig.base.json", base), ("tsconfig.json", child)]),
        );

        // Child override `@/*` -> `app/*`.
        assert_eq!(
            resolver.resolve_import("app/main.ts", "@/services/user"),
            Some("app/services/user.ts".to_string())
        );
        // Inherited `@shared/*` -> `packages/shared/*` (resolved relative to inherited `baseUrl: "src"`).
        assert_eq!(
            resolver.resolve_import("app/main.ts", "@shared/types"),
            Some("packages/shared/types.ts".to_string())
        );
        // Child `@api` joined with inherited `baseUrl: "src"`.
        assert_eq!(
            resolver.resolve_import("app/main.ts", "@api"),
            Some("src/api/client.ts".to_string())
        );
        // Inherited `baseUrl: "src"` direct import.
        assert_eq!(
            resolver.resolve_import("app/main.ts", "models/account"),
            Some("src/models/account.ts".to_string())
        );
    }

    #[test]
    fn tsconfig_extends_handles_cycles_subdirectories_and_missing_external_configs() {
        let cyclic_a = r#"{
            "extends": "./tsconfig.b.json",
            "compilerOptions": {
                "paths": {
                    "@a/*": ["src/a/*"]
                }
            }
        }"#;
        let cyclic_b = r#"{
            "extends": ["./tsconfig.json", "@vue/tsconfig/tsconfig.dom.json"],
            "compilerOptions": {
                "paths": {
                    "@b/*": ["src/b/*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/a/one.ts", "src/b/two.ts"]),
            &configs(&[("tsconfig.json", cyclic_a), ("tsconfig.b.json", cyclic_b)]),
        );

        assert_eq!(
            resolver.resolve_import("src/index.ts", "@a/one"),
            Some("src/a/one.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/index.ts", "@b/two"),
            Some("src/b/two.ts".to_string())
        );

        // Subdirectory base config with `${configDir}` anchored to root config dir.
        let sub_base = r#"{
            "compilerOptions": {
                "baseUrl": "../src",
                "paths": {
                    "@/*": ["./*"],
                    "@root/*": ["${configDir}/shared/*"]
                }
            }
        }"#;
        let root_cfg = r#"{
            "extends": "./configs/tsconfig.base.json"
        }"#;

        let sub_resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/foo.ts", "shared/bar.ts"]),
            &configs(&[
                ("configs/tsconfig.base.json", sub_base),
                ("tsconfig.json", root_cfg),
            ]),
        );

        assert_eq!(
            sub_resolver.resolve_import("src/index.ts", "@/foo"),
            Some("src/foo.ts".to_string())
        );
        assert_eq!(
            sub_resolver.resolve_import("src/index.ts", "@root/bar"),
            Some("shared/bar.ts".to_string())
        );
    }

    #[test]
    fn falls_back_to_jsconfig_when_tsconfig_is_absent() {
        let jsconfig = r#"{
            "compilerOptions": {
                "paths": {
                    "@/*": ["web/*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&["web/api/users.js"]),
            &configs(&[("jsconfig.json", jsconfig)]),
        );

        assert_eq!(
            resolver.resolve_import("web/App.jsx", "@/api/users"),
            Some("web/api/users.js".to_string())
        );
    }

    #[test]
    fn smart_fallback_aliases_work_for_nuxt_vite_next_without_tsconfig() {
        // Nuxt 4 layout (`app/`).
        let nuxt_resolver = ModuleResolver::from_git_snapshot(
            &files(&["app/services/user.ts", "app/pages/index.vue"]),
            &BTreeMap::new(),
        );
        assert_eq!(
            nuxt_resolver.resolve_import("app/pages/index.vue", "~/services/user"),
            Some("app/services/user.ts".to_string())
        );
        assert_eq!(
            nuxt_resolver.resolve_import("app/pages/index.vue", "@/services/user"),
            Some("app/services/user.ts".to_string())
        );

        // Vite / Next.js layout (`src/`).
        let vite_resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/services/user.ts", "src/components/App.tsx"]),
            &BTreeMap::new(),
        );
        assert_eq!(
            vite_resolver.resolve_import("src/components/App.tsx", "@/services/user"),
            Some("src/services/user.ts".to_string())
        );
        assert_eq!(
            vite_resolver.resolve_import("src/components/App.tsx", "~/services/user"),
            Some("src/services/user.ts".to_string())
        );

        // Root-level layout (`""`) and priority `app/` > `src/` > `""`.
        let root_resolver = ModuleResolver::from_git_snapshot(
            &files(&["services/user.ts", "pages/home.tsx"]),
            &BTreeMap::new(),
        );
        assert_eq!(
            root_resolver.resolve_import("pages/home.tsx", "@/services/user"),
            Some("services/user.ts".to_string())
        );

        // Explicit tsconfig alias for `@/*` is respected and not overridden by fallback,
        // while unmatched `~/*` still falls back.
        let explicit_cfg = r#"{
            "compilerOptions": {
                "paths": {
                    "@/*": ["custom/*"]
                }
            }
        }"#;
        let explicit_resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/services/user.ts", "custom/only.ts"]),
            &configs(&[("tsconfig.json", explicit_cfg)]),
        );
        assert_eq!(
            explicit_resolver.resolve_import("custom/only.ts", "@/services/user"),
            None
        );
        assert_eq!(
            explicit_resolver.resolve_import("custom/only.ts", "~/services/user"),
            Some("src/services/user.ts".to_string())
        );
    }

    #[test]
    fn bare_npm_packages_and_invalid_paths_return_none() {
        let tsconfig = r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@/*": ["src/*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/App.tsx", "src/services/user.ts"]),
            &configs(&[("tsconfig.json", tsconfig)]),
        );

        for bare in [
            "react",
            "vue",
            "axios",
            "zod",
            "@tanstack/react-query",
            "@vueuse/core",
            "node:fs",
            "https://cdn.example.com/lib.js",
            "",
        ] {
            assert_eq!(
                resolver.resolve_import("src/App.tsx", bare),
                None,
                "expected bare/external specifier `{bare}` to return None"
            );
        }

        // Relative path escaping repo root must return None.
        assert_eq!(
            resolver.resolve_import("src/App.tsx", "../../../outside"),
            None
        );
    }

    #[test]
    fn normalizes_windows_separators_query_strings_and_longest_prefix_order() {
        let tsconfig = r#"{
            "compilerOptions": {
                "paths": {
                    "@/*": ["src/*"],
                    "@/special/*": ["override/*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                ".\\src\\special\\client.ts",
                "override\\client.ts",
                "src\\components\\UserCard.vue",
            ]),
            &configs(&[("./tsconfig.json", tsconfig)]),
        );

        // Longest static prefix (`@/special/*`) deterministically beats `@/*`.
        assert_eq!(
            resolver.resolve_import("src\\App.tsx", "@/special/client"),
            Some("override/client.ts".to_string())
        );
        // Query string stripped cleanly (e.g. Vite `?raw`).
        assert_eq!(
            resolver.resolve_import("src/App.tsx", "./components/UserCard.vue?raw"),
            Some("src/components/UserCard.vue".to_string())
        );
    }

    #[test]
    fn resolves_nuxt_generated_tsconfig_extends_and_dotted_stems() {
        let root_tsconfig = r#"{
            "extends": "./.nuxt/tsconfig.json"
        }"#;
        let nuxt_tsconfig = r#"{
            "compilerOptions": {
                "paths": {
                    "~/*": ["../app/*"],
                    "@/*": ["../app/*"],
                    "~~/*": ["../*"]
                }
            }
        }"#;

        let resolver = ModuleResolver::from_git_snapshot(
            &files(&[
                "app/services/user.service.ts",
                "app/pages/index.vue",
                "server/api/users.ts",
            ]),
            &configs(&[
                ("tsconfig.json", root_tsconfig),
                (".nuxt/tsconfig.json", nuxt_tsconfig),
            ]),
        );

        assert_eq!(
            resolver.resolve_import("app/pages/index.vue", "~/services/user.service"),
            Some("app/services/user.service.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("app/pages/index.vue", "~~/server/api/users"),
            Some("server/api/users.ts".to_string())
        );
    }

    #[test]
    fn handles_malformed_tsconfig_and_jsonc_string_escapes_gracefully() {
        let broken = r#"{ "compilerOptions": { "paths": [ invalid "#;
        let resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/services/user.ts", "src/App.tsx"]),
            &configs(&[("tsconfig.json", broken)]),
        );
        // Falls back cleanly to relative & smart aliases without panicking.
        assert_eq!(
            resolver.resolve_import("src/App.tsx", "./services/user"),
            Some("src/services/user.ts".to_string())
        );
        assert_eq!(
            resolver.resolve_import("src/App.tsx", "@/services/user"),
            Some("src/services/user.ts".to_string())
        );

        // Escaped quotes, backslashes, and unicode escapes in JSONC.
        let escaped = r#"{
            "description": "quote \" and slash \\ and unicode \u0040 \uD83D\uDE80",
            "compilerOptions": {
                "paths": {
                    "\u0040/*": ["src/*"]
                }
            }
        }"#;
        let escaped_resolver = ModuleResolver::from_git_snapshot(
            &files(&["src/services/user.ts"]),
            &configs(&[("tsconfig.json", escaped)]),
        );
        assert_eq!(
            escaped_resolver.resolve_import("src/App.tsx", "@/services/user"),
            Some("src/services/user.ts".to_string())
        );
    }
}
