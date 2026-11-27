mod selector;

use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, Parser, Subcommand, ValueEnum};
use laine_cli::engine::{
    AnalyzeRequest, ConsumerRevision, ConsumersRequest, catalog_workspace_routes, index_workspace,
    query_endpoint_consumers, run_analysis_with_consumer_revision,
};
use laine_cli::report::{
    omit_consumers_snippets, omit_snippets, render_consumers_json, render_consumers_markdown,
    render_consumers_terminal, render_json, render_markdown, render_terminal,
};
use laine_core::config::{
    CustomClientConfig, PolicyConfig, RouteFramework, RouteSourceConfig, ServiceConfig,
    WorkspaceConfig, lock_config_update, write_config_atomically,
};
use laine_core::error::LaineError;
use laine_core::fixtures::create_demo_workspace;
use laine_core::git::{GitRepo, STAGED_REV};
use laine_core::model::HttpMethod;
use laine_core::workspace::{
    ClientAmbiguity, DiscoveredRepo, ResolvedContext, WorkspaceRegistry, build_workspace_config,
    discover_workspace_clients_with, infer_workspace_scan_dir, inspect_repo_path,
    merge_workspace_config, normalize_absolute_path, resolve_context_from_cwd,
    same_repository_path, scan_directory, validate_workspace_name, workspace_service_path,
};
use selector::{ensure_selection_terminal, select_repositories};

#[derive(Debug, Parser)]
#[command(
    name = "laine",
    version,
    about = "Cross-repository change impact analyzer"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize a laine.toml configuration file (optionally scanning a directory of Git repos)
    Init {
        /// Destination path for the configuration file (defaults to ~/.laine/configs/<workspace>.toml when --workspace is set, else laine.toml)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite an existing configuration file
        #[arg(long)]
        force: bool,
        /// Scan a directory for local Git repositories (defaults to `.` when passed without a path)
        #[arg(long, num_args = 0..=1, default_missing_value = ".")]
        scan: Option<PathBuf>,
        /// Comma-separated repository names to include from `--scan` (non-interactive)
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        /// Include all repositories discovered by `--scan` without prompting
        #[arg(long)]
        all: bool,
        /// Optional workspace name to register in ~/.laine/workspaces.toml for zero-flag `laine check`
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Manage named workspaces registered in ~/.laine/workspaces.toml
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Manage service repositories in laine.toml
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Materialize the reproducible 4-repository Git demo workspace
    DemoSetup {
        /// Target directory for the generated demo workspace
        #[arg(default_value = "fixtures/demo-workspace")]
        path: PathBuf,
    },
    /// Pre-compute and cache consumer repository AST extractions in SQLite
    Index {
        /// Path to laine.toml
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
    },
    /// Analyze local changes from inside any registered repository (auto-detects workspace & repo)
    Check {
        /// Optional path to laine.toml (auto-resolved from cwd or ~/.laine/workspaces.toml if omitted)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Optional named workspace from ~/.laine/workspaces.toml
        #[arg(long)]
        workspace: Option<String>,
        /// Optional service repository name (auto-detected from cwd if omitted)
        #[arg(long)]
        repo: Option<String>,
        /// Base Git revision before change
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Head Git revision after change (defaults to WORKTREE; overridden by --staged)
        #[arg(long, default_value = "WORKTREE")]
        head: String,
        /// Check only staged changes (shortcut for `--head STAGED`)
        #[arg(long)]
        staged: bool,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,
        /// Bypass the SQLite incremental extraction cache
        #[arg(long)]
        no_cache: bool,
        /// Suppress evidence snippets and detailed errors in publishable reports
        #[arg(long)]
        no_snippets: bool,
        /// Include the local checkout roots in the JSON report
        #[arg(long)]
        include_repo_roots: bool,
        /// Revision of consumer checkouts, including client code in the producer repo
        #[arg(long, value_enum, ignore_case = true, default_value_t = ConsumerRevision::Head)]
        consumer_revision: ConsumerRevision,
    },
    /// List direct and indirect consumers of a service endpoint
    Consumers(ConsumersArgs),
    /// Analyze cross-repository impact between two Git revisions
    Analyze {
        /// Path to laine.toml
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
        /// Service repository where the change occurred
        #[arg(long)]
        repo: String,
        /// Base Git revision before change (commit/branch/tag, or HEAD/STAGED/WORKTREE)
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Head Git revision after change (commit/branch/tag, STAGED, or WORKTREE for local uncommitted changes)
        #[arg(long, default_value = "WORKTREE")]
        head: String,
        /// Output format
        #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
        format: OutputFormat,
        /// Bypass the SQLite incremental extraction cache
        #[arg(long)]
        no_cache: bool,
        /// Suppress evidence snippets and detailed errors in publishable reports
        #[arg(long)]
        no_snippets: bool,
        /// Include the local checkout roots in the JSON report
        #[arg(long)]
        include_repo_roots: bool,
        /// Revision of consumer checkouts, including client code in the producer repo
        #[arg(long, value_enum, ignore_case = true, default_value_t = ConsumerRevision::Head)]
        consumer_revision: ConsumerRevision,
    },
}

#[derive(Debug, Args)]
struct ConsumersArgs {
    /// Path to laine.toml, discovered from the current directory when omitted
    #[arg(long)]
    config: Option<PathBuf>,
    /// Named workspace from the local workspace registry
    #[arg(long)]
    workspace: Option<String>,
    /// Producer service name, inferred from the current directory when omitted
    #[arg(long)]
    service: Option<String>,
    /// HTTP method (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS, TRACE)
    #[arg(long, value_parser = parse_http_method)]
    method: HttpMethod,
    /// Route template or concrete path, such as /customers/:id or /customers/42
    #[arg(long)]
    path: String,
    /// Response field path, such as fullName or address.city
    #[arg(long)]
    field: Option<String>,
    /// Producer catalog revision (commit, branch, tag, STAGED, or WORKTREE)
    #[arg(long, default_value = "HEAD")]
    producer_revision: String,
    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    format: OutputFormat,
    /// Bypass the SQLite incremental extraction cache
    #[arg(long)]
    no_cache: bool,
    /// Suppress evidence snippets and detailed errors
    #[arg(long)]
    no_snippets: bool,
    /// Include local checkout roots in the JSON report
    #[arg(long)]
    include_repo_roots: bool,
    /// Revision of consumer checkouts, including client code in the producer repo
    #[arg(long, value_enum, ignore_case = true, default_value_t = ConsumerRevision::Head)]
    consumer_revision: ConsumerRevision,
}

#[derive(Debug, Subcommand)]
enum WorkspaceAction {
    /// List registered named workspaces
    List,
    /// Register an existing laine.toml under a workspace name
    Add {
        /// Short name for the workspace (e.g. `corporativo`)
        name: String,
        /// Path to the workspace's laine.toml file
        config: PathBuf,
        /// Replace an existing registration with the same name
        #[arg(long)]
        replace: bool,
    },
    /// Interactively add or remove repositories in an existing workspace
    Edit {
        /// Workspace name registered in ~/.laine/workspaces.toml
        name: String,
        /// Optional directory to scan for sibling repositories (auto-inferred if omitted)
        #[arg(long)]
        scan: Option<PathBuf>,
        /// Comma-separated repository names to set as the workspace selection (non-interactive)
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        /// Select all repositories found in the scan directory without prompting
        #[arg(long)]
        all: bool,
    },
    /// Unregister a named workspace (does not delete its laine.toml file)
    Remove {
        /// Workspace name to remove
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum RepoAction {
    /// Register a local Git service repository in laine.toml or a named workspace
    Add {
        /// Path to the local Git repository
        path: PathBuf,
        /// Unique service name (defaults to the folder name)
        #[arg(long)]
        name: Option<String>,
        /// Path to laine.toml (ignored when `--workspace` is provided)
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
        /// Optional registered workspace name to update instead of `--config`
        #[arg(long)]
        workspace: Option<String>,
        /// Optional OpenAPI spec path relative to the service repository
        #[arg(long)]
        openapi: Option<PathBuf>,
        /// Base URL(s) belonging to this service (repeatable)
        #[arg(long = "base-url")]
        base_urls: Vec<String>,
        /// Optional backend route framework when extracting routes from code
        #[arg(long, value_enum)]
        framework: Option<CliRouteFramework>,
    },
    /// Remove a service repository from laine.toml or a named workspace
    Remove {
        /// Service name to remove
        name: String,
        /// Path to laine.toml (ignored when `--workspace` is provided)
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
        /// Optional registered workspace name to update instead of `--config`
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliRouteFramework {
    Express,
    Laravel,
    FastApi,
    Flask,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Terminal,
    Json,
    Markdown,
}

#[derive(Clone, Copy)]
struct AnalysisOutput {
    format: OutputFormat,
    no_cache: bool,
    no_snippets: bool,
    include_repo_roots: bool,
    consumer_revision: ConsumerRevision,
}

const USAGE_EXIT_CODE: u8 = 64;

const STARTER_CONFIG: &str = r#"# Laine cross-repository impact analyzer configuration
version = 1

# Optional environment variables used to resolve `process.env.*` / `import.meta.env.*`
[env]
# API_BASE_URL = "http://customer-service"

# Register each repository participating in the dependency graph:
# [[services]]
# name = "customer-service"
# path = "./customer-service"
# openapi = "openapi.yaml"
# base_urls = ["http://customer-service"]

# Optional custom HTTP wrappers (e.g. `$fetch`, `getClient().fetch`, `apiClient`):
# [[clients]]
# callee = "apiClient"
# target_service = "customer-service"
"#;

// Parse CLI arguments and dispatch to the selected command.
fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(cli) => cli,
        Err(exit) => return exit,
    };

    match cli.command {
        Commands::Init {
            path,
            force,
            scan,
            select,
            all,
            workspace,
        } => handle_init(path, force, scan, select, all, workspace),
        Commands::Workspace { action } => handle_workspace(action),
        Commands::Repo { action } => handle_repo(action),
        Commands::DemoSetup { path } => match create_demo_workspace(&path) {
            Ok(paths) => {
                println!("Demo workspace created at: {}", paths.root.display());
                println!("  Config:               {}", paths.config_path.display());
                println!(
                    "  customer-service:     {}",
                    paths.customer_service.display()
                );
                println!(
                    "  billing-service:      {}",
                    paths.billing_service.display()
                );
                println!(
                    "  notification-service: {}",
                    paths.notification_service.display()
                );
                println!(
                    "  shipping-service:     {}",
                    paths.shipping_service.display()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Error creating demo workspace: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Index { config } => {
            let started = Instant::now();
            match index_workspace(&config) {
                Ok(summary) => {
                    println!(
                        "Indexed {} repo(s) in {} ms ({} cache hit(s), {} cache miss(es)) -> {}",
                        summary.repos_indexed,
                        started.elapsed().as_millis(),
                        summary.cache_hits,
                        summary.cache_misses,
                        summary.cache_path.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Index error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Check {
            config,
            workspace,
            repo,
            base,
            head,
            staged,
            format,
            no_cache,
            no_snippets,
            include_repo_roots,
            consumer_revision,
        } => {
            let output = AnalysisOutput {
                format,
                no_cache,
                no_snippets,
                include_repo_roots,
                consumer_revision,
            };
            let resolved = match resolve_command_context(
                config.as_deref(),
                workspace.as_deref(),
                repo.as_deref(),
            ) {
                Ok(ctx) => ctx,
                Err(err) => {
                    print_analysis_error("Check", &err, output);
                    return ExitCode::FAILURE;
                }
            };

            let effective_head = if staged { STAGED_REV.to_string() } else { head };
            execute_analysis(
                AnalyzeRequest {
                    config_path: resolved.config_path,
                    target_repo: resolved.repo_name,
                    base_rev: base,
                    head_rev: effective_head,
                },
                output,
            )
        }
        Commands::Consumers(args) => execute_consumers(args),
        Commands::Analyze {
            config,
            repo,
            base,
            head,
            format,
            no_cache,
            no_snippets,
            include_repo_roots,
            consumer_revision,
        } => execute_analysis(
            AnalyzeRequest {
                config_path: config,
                target_repo: repo,
                base_rev: base,
                head_rev: head,
            },
            AnalysisOutput {
                format,
                no_cache,
                no_snippets,
                include_repo_roots,
                consumer_revision,
            },
        ),
    }
}

// Preserve Clap help and render structured usage errors for report commands.
fn parse_cli() -> Result<Cli, ExitCode> {
    let args = env::args_os().collect::<Vec<_>>();
    match Cli::try_parse_from(&args) {
        Ok(cli) => Ok(cli),
        Err(error) => {
            let command = args.get(1).and_then(|command| command.to_str());
            if !matches!(command, Some("analyze" | "check" | "consumers")) || !error.use_stderr() {
                let _ = error.print();
                return Err(ExitCode::from(error.exit_code() as u8));
            }

            let mut args = args.iter().skip(2).take_while(|arg| *arg != "--");
            let mut json = false;
            while let Some(arg) = args.next() {
                if arg == "--format=json"
                    || (arg == "--format" && args.next().is_some_and(|value| value == "json"))
                {
                    json = true;
                    break;
                }
            }
            if json {
                let message = if command == Some("consumers") {
                    "Invalid consumers arguments. Run the command with --help."
                } else {
                    "Invalid analysis arguments. Run the command with --help."
                };
                print_json_error(
                    USAGE_EXIT_CODE,
                    "invalid_arguments",
                    "check_command_help",
                    message,
                );
            } else {
                let _ = error.print();
            }
            Err(ExitCode::from(USAGE_EXIT_CODE))
        }
    }
}

// Validate the method during CLI parsing so invalid values use the same error protocol.
fn parse_http_method(value: &str) -> Result<HttpMethod, String> {
    HttpMethod::parse(value)
        .ok_or_else(|| "expected GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS, or TRACE".to_owned())
}

// Resolve the current workspace and service from flags or the checkout directory.
fn resolve_command_context(
    config: Option<&Path>,
    workspace: Option<&str>,
    service: Option<&str>,
) -> Result<ResolvedContext, LaineError> {
    let cwd = env::current_dir().map_err(|source| LaineError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let registry = WorkspaceRegistry::default_path()?;
    resolve_context_from_cwd(&cwd, config, workspace, service, &registry)
}

/// Initialize the workspace, checking terminal support before interactive selection.
fn handle_init(
    path: Option<PathBuf>,
    force: bool,
    scan: Option<PathBuf>,
    select: Vec<String>,
    all: bool,
    workspace: Option<String>,
) -> ExitCode {
    if let Some(name) = workspace.as_deref()
        && let Err(err) = validate_workspace_name(name)
    {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    let dest = match path {
        Some(explicit) => explicit,
        None => match workspace.as_deref() {
            Some(ws) => match WorkspaceRegistry::default_config_path(ws) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("Workspace config path error: {err}");
                    return ExitCode::FAILURE;
                }
            },
            None => PathBuf::from("laine.toml"),
        },
    };

    if dest.exists() && !force {
        eprintln!(
            "Config file {} already exists (use --force to overwrite)",
            dest.display()
        );
        return ExitCode::FAILURE;
    }

    if let Some(scan_dir) = scan {
        if !all
            && select.is_empty()
            && let Err(message) = ensure_selection_terminal()
        {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }

        let discovered = match scan_directory(&scan_dir) {
            Ok(repos) => repos,
            Err(err) => {
                eprintln!("Scan error: {err}");
                return ExitCode::FAILURE;
            }
        };

        if discovered.is_empty() {
            eprintln!("No Git repositories found in {}", scan_dir.display());
            return ExitCode::FAILURE;
        }

        let selected = if all {
            discovered
        } else if !select.is_empty() {
            match filter_selected_repos(&discovered, &select) {
                Ok(repos) => repos,
                Err(msg) => {
                    eprintln!("{msg}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            match select_repositories(&discovered, &[], "Select repositories for this workspace") {
                Ok(repos) => repos,
                Err(msg) => {
                    eprintln!("{msg}");
                    return ExitCode::FAILURE;
                }
            }
        };

        if selected.is_empty() {
            eprintln!("No repositories selected.");
            return ExitCode::FAILURE;
        }

        let config_dir = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut cfg = build_workspace_config(&selected, config_dir, false);
        let ambiguities = discover_clients(&mut cfg, config_dir);
        if let Err(err) =
            persist_initialized_workspace(&dest, workspace.as_deref(), force, Some(&cfg))
        {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }

        println!(
            "Initialized {} with {} repository(ies):",
            dest.display(),
            selected.len()
        );
        for repo in &selected {
            println!("  - {} ({})", repo.name, repo.detected_kind);
        }
        print_workspace_clients(&cfg.clients, "Auto-detected", &ambiguities);
        if !cfg.env.is_empty() {
            println!("Auto-detected {} API URL env var(s).", cfg.env.len());
        }
    } else if let Err(err) = persist_initialized_workspace(&dest, workspace.as_deref(), force, None)
    {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    } else {
        println!("Initialized {}", dest.display());
    }

    if let Some(ws_name) = workspace {
        println!("Registered workspace '{ws_name}' -> {}", dest.display());
    }

    ExitCode::SUCCESS
}

// Lee el registro y serializa los cambios de nombres entre procesos.
fn handle_workspace(action: WorkspaceAction) -> ExitCode {
    let registry_path = match WorkspaceRegistry::default_path() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("Workspace registry error: {err}");
            return ExitCode::FAILURE;
        }
    };

    match action {
        WorkspaceAction::List => match WorkspaceRegistry::load_from_path(&registry_path) {
            Ok(reg) => {
                if reg.workspaces.is_empty() {
                    println!(
                        "No workspaces registered yet ({}).",
                        registry_path.display()
                    );
                } else {
                    for (name, path) in &reg.workspaces {
                        println!("{name}\t{}", path.display());
                    }
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Failed to read workspace registry: {err}");
                ExitCode::FAILURE
            }
        },
        WorkspaceAction::Add {
            name,
            config,
            replace,
        } => {
            if !config.is_file() {
                eprintln!("Config file does not exist: {}", config.display());
                return ExitCode::FAILURE;
            }
            if let Err(err) = WorkspaceConfig::load_from_path(&config) {
                eprintln!("Invalid Laine config {}: {err}", config.display());
                return ExitCode::FAILURE;
            }
            match register_workspace(&registry_path, &name, &config, replace) {
                Ok(()) => {
                    println!("Registered workspace '{name}' -> {}", config.display());
                    ExitCode::SUCCESS
                }
                Err(msg) => {
                    eprintln!("{msg}");
                    ExitCode::FAILURE
                }
            }
        }
        WorkspaceAction::Edit {
            name,
            scan,
            select,
            all,
        } => handle_workspace_edit(&name, scan, select, all, &registry_path),
        WorkspaceAction::Remove { name } => {
            let Ok(_lock) = lock_cli_update(&registry_path) else {
                return ExitCode::FAILURE;
            };
            let mut reg = match WorkspaceRegistry::load_from_path(&registry_path) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("Failed to load workspace registry: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if !reg.remove(&name) {
                eprintln!("Workspace '{name}' is not registered.");
                return ExitCode::FAILURE;
            }
            if let Err(err) = reg.save_to_path(&registry_path) {
                eprintln!("Failed to save workspace registry: {err}");
                return ExitCode::FAILURE;
            }
            println!("Removed workspace '{name}'.");
            ExitCode::SUCCESS
        }
    }
}

/// Update a workspace's repositories from an explicit selection or the interactive picker.
fn handle_workspace_edit(
    name: &str,
    scan: Option<PathBuf>,
    select: Vec<String>,
    all: bool,
    registry_path: &Path,
) -> ExitCode {
    if !all
        && select.is_empty()
        && let Err(message) = ensure_selection_terminal()
    {
        eprintln!("{message}");
        return ExitCode::FAILURE;
    }

    let reg = match WorkspaceRegistry::load_from_path(registry_path) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("Failed to load workspace registry: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(cfg_path) = reg.workspaces.get(name) else {
        eprintln!("Workspace '{name}' is not registered.");
        return ExitCode::FAILURE;
    };
    let Ok(_lock) = lock_cli_update(cfg_path) else {
        return ExitCode::FAILURE;
    };
    let existing = match WorkspaceConfig::load_from_path(cfg_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Failed to load {}: {err}", cfg_path.display());
            return ExitCode::FAILURE;
        }
    };

    let scan_dir = match scan.or_else(|| infer_workspace_scan_dir(&existing, cfg_path)) {
        Some(dir) => dir,
        None => {
            eprintln!(
                "Could not infer a scan directory for workspace '{name}'; pass `--scan <dir>`."
            );
            return ExitCode::FAILURE;
        }
    };

    let mut discovered = match scan_directory(&scan_dir) {
        Ok(repos) => repos,
        Err(err) => {
            eprintln!("Scan error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Keep any already-configured service that lives outside `scan_dir` in the candidate list.
    let cfg_dir = cfg_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut preselected_paths = Vec::new();
    for svc in &existing.services {
        let svc_abs = workspace_service_path(cfg_dir, svc);
        if let Some(repo) = discovered
            .iter_mut()
            .find(|repo| same_repository_path(&repo.path, &svc_abs))
        {
            repo.name.clone_from(&svc.name);
        } else {
            let mut repo = inspect_repo_path(&svc_abs);
            repo.name.clone_from(&svc.name);
            discovered.push(repo);
        }
        preselected_paths.push(svc_abs);
    }
    discovered.sort_by(|a, b| a.name.cmp(&b.name));

    let selected = if all {
        discovered
    } else if !select.is_empty() {
        match filter_selected_repos(&discovered, &select) {
            Ok(repos) => repos,
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        match select_repositories(
            &discovered,
            &preselected_paths,
            &format!("Edit repositories in workspace '{name}'"),
        ) {
            Ok(repos) => repos,
            Err(msg) => {
                eprintln!("{msg}");
                return ExitCode::FAILURE;
            }
        }
    };

    if selected.is_empty() {
        eprintln!("No repositories selected (workspace unchanged).");
        return ExitCode::FAILURE;
    }

    let mut updated = merge_workspace_config(&existing, &selected, cfg_dir);
    let mut names = HashSet::new();
    for service in &updated.services {
        if !names.insert(&service.name) {
            eprintln!(
                "Multiple repositories would use the service name '{}'. Rename the existing service or select fewer repositories.",
                service.name
            );
            return ExitCode::FAILURE;
        }
    }
    let ambiguities = discover_clients(&mut updated, cfg_dir);
    if let Err(err) = updated.save_to_path(cfg_path) {
        eprintln!("Failed to save {}: {err}", cfg_path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "Updated workspace '{name}' ({}) with {} repository(ies):",
        cfg_path.display(),
        updated.services.len()
    );
    for svc in &updated.services {
        println!("  - {}", svc.name);
    }
    print_workspace_clients(&updated.clients, "Configured", &ambiguities);
    ExitCode::SUCCESS
}

// Discover missing clients with the same framework extractors used by analysis.
fn discover_clients(config: &mut WorkspaceConfig, config_dir: &Path) -> Vec<ClientAmbiguity> {
    discover_workspace_clients_with(config, config_dir, catalog_workspace_routes)
}

// Print scoped mappings and show the producer choices for unresolved ties.
fn print_workspace_clients(
    clients: &[CustomClientConfig],
    verb: &str,
    ambiguities: &[ClientAmbiguity],
) {
    if !clients.is_empty() {
        println!("{verb} HTTP client wrapper(s):");
        for client in clients {
            let consumer = client
                .consumer_service
                .as_deref()
                .unwrap_or("workspace-wide");
            let prefix_note = client
                .path_prefix
                .as_deref()
                .map(|p| format!(" (prefix: {p})"))
                .unwrap_or_default();
            println!(
                "  - {consumer}: `{}` -> {}{prefix_note}",
                client.callee, client.target_service
            );
        }
    }
    for ambiguity in ambiguities {
        eprintln!(
            "Ambiguous HTTP client wrapper `{}` in '{}': candidates: {}. Set consumer_service and target_service in [[clients]] to choose a producer.",
            ambiguity.callee,
            ambiguity.consumer,
            ambiguity.candidates.join(", ")
        );
    }
}

// Update repositories and client mappings under the same config write lock.
fn handle_repo(action: RepoAction) -> ExitCode {
    match action {
        RepoAction::Add {
            path,
            name,
            config,
            workspace,
            openapi,
            base_urls,
            framework,
        } => {
            let repo = match GitRepo::discover(&path) {
                Ok(repo) => repo,
                Err(err) => {
                    eprintln!("Cannot add Git repository at {}: {err}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            let target_cfg_path = match resolve_repo_command_config(config, workspace.as_deref()) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("{msg}");
                    return ExitCode::FAILURE;
                }
            };

            let Ok(_lock) = lock_cli_update(&target_cfg_path) else {
                return ExitCode::FAILURE;
            };
            let service_name = name.unwrap_or_else(|| {
                repo.root()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "service".to_string())
            });

            let mut cfg = if target_cfg_path.exists() {
                match WorkspaceConfig::load_from_path(&target_cfg_path) {
                    Ok(c) => c,
                    Err(err) => {
                        eprintln!("Failed to load {}: {err}", target_cfg_path.display());
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                WorkspaceConfig {
                    version: 1,
                    services: Vec::new(),
                    clients: Vec::new(),
                    env: Default::default(),
                    policy: PolicyConfig::default(),
                    cache_path: None,
                }
            };

            if cfg.find_service(&service_name).is_some() {
                eprintln!(
                    "Service '{}' already exists in {}",
                    service_name,
                    target_cfg_path.display()
                );
                return ExitCode::FAILURE;
            }

            let routes = framework.map(|fw| RouteSourceConfig {
                framework: match fw {
                    CliRouteFramework::Express => RouteFramework::Express,
                    CliRouteFramework::Laravel => RouteFramework::Laravel,
                    CliRouteFramework::FastApi => RouteFramework::FastApi,
                    CliRouteFramework::Flask => RouteFramework::Flask,
                },
                files: Vec::new(),
                prefix: None,
            });

            cfg.services.push(ServiceConfig {
                name: service_name.clone(),
                path: repo_path_for_config(repo.root(), &target_cfg_path),
                openapi,
                routes,
                base_urls,
            });

            let config_dir = target_cfg_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let existing_clients = cfg.clients.len();
            let ambiguities = discover_clients(&mut cfg, config_dir);
            match cfg.save_to_path(&target_cfg_path) {
                Ok(()) => {
                    println!(
                        "Added service '{service_name}' to {}",
                        target_cfg_path.display()
                    );
                    print_workspace_clients(
                        &cfg.clients[existing_clients..],
                        "Configured",
                        &ambiguities,
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to save {}: {err}", target_cfg_path.display());
                    ExitCode::FAILURE
                }
            }
        }
        RepoAction::Remove {
            name,
            config,
            workspace,
        } => {
            let target_cfg_path = match resolve_repo_command_config(config, workspace.as_deref()) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("{msg}");
                    return ExitCode::FAILURE;
                }
            };

            let Ok(_lock) = lock_cli_update(&target_cfg_path) else {
                return ExitCode::FAILURE;
            };
            let mut cfg = match WorkspaceConfig::load_from_path(&target_cfg_path) {
                Ok(c) => c,
                Err(err) => {
                    eprintln!("Failed to load {}: {err}", target_cfg_path.display());
                    return ExitCode::FAILURE;
                }
            };

            let before = cfg.services.len();
            cfg.services.retain(|s| s.name != name);
            if cfg.services.len() == before {
                eprintln!(
                    "Service '{name}' was not found in {}",
                    target_cfg_path.display()
                );
                return ExitCode::FAILURE;
            }
            cfg.clients.retain(|client| {
                client.target_service != name && client.consumer_service.as_deref() != Some(&name)
            });

            match cfg.save_to_path(&target_cfg_path) {
                Ok(()) => {
                    println!(
                        "Removed service '{name}' from {}",
                        target_cfg_path.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to save {}: {err}", target_cfg_path.display());
                    ExitCode::FAILURE
                }
            }
        }
    }
}

// Usa una ruta relativa si el checkout está dentro del directorio del TOML.
fn repo_path_for_config(root: &Path, config: &Path) -> PathBuf {
    let config_dir = normalize_absolute_path(
        config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    );
    match root.strip_prefix(config_dir) {
        Ok(path) if path.as_os_str().is_empty() => PathBuf::from("."),
        Ok(path) => path.to_path_buf(),
        Err(_) => root.to_path_buf(),
    }
}

fn resolve_repo_command_config(
    config: PathBuf,
    workspace: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(ws_name) = workspace {
        let registry_path = WorkspaceRegistry::default_path()
            .map_err(|e| format!("Workspace registry error: {e}"))?;
        let reg = WorkspaceRegistry::load_from_path(&registry_path)
            .map_err(|e| format!("Failed to load workspace registry: {e}"))?;
        reg.workspaces
            .get(ws_name)
            .cloned()
            .ok_or_else(|| format!("Workspace '{ws_name}' is not registered."))
    } else {
        Ok(config)
    }
}

// Conserva el bloqueo mientras se lee, modifica y reemplaza el registro.
fn register_workspace(
    registry_path: &Path,
    name: &str,
    config_path: &Path,
    replace: bool,
) -> Result<(), String> {
    ensure_distinct_config(registry_path, config_path)?;
    let _lock = lock_config_update(registry_path).map_err(|err| err.to_string())?;
    let mut reg = WorkspaceRegistry::load_from_path(registry_path)
        .map_err(|err| format!("Failed to load workspace registry: {err}"))?;
    reg.register(name, config_path.to_path_buf(), replace)
        .map_err(|err| err.to_string())?;
    reg.save_to_path(registry_path)
        .map_err(|err| format!("Failed to save workspace registry: {err}"))
}

// Valida el alta bajo bloqueo y guarda el archivo antes de publicar su entrada en el registro.
fn persist_initialized_workspace(
    dest: &Path,
    workspace: Option<&str>,
    force: bool,
    config: Option<&WorkspaceConfig>,
) -> Result<(), String> {
    let registration = if let Some(name) = workspace {
        let path = WorkspaceRegistry::default_path().map_err(|err| err.to_string())?;
        ensure_distinct_config(&path, dest)?;
        let lock = lock_config_update(&path).map_err(|err| err.to_string())?;
        let mut registry =
            WorkspaceRegistry::load_from_path(&path).map_err(|err| err.to_string())?;
        registry
            .register(name, dest.to_path_buf(), force)
            .map_err(|err| err.to_string())?;
        Some((path, registry, lock))
    } else {
        None
    };

    let _lock = lock_config_update(dest).map_err(|err| err.to_string())?;
    if dest.exists() && !force {
        return Err(format!(
            "Config file {} already exists (use --force to overwrite)",
            dest.display()
        ));
    }
    if let Some(config) = config {
        config.save_to_path(dest)
    } else {
        write_config_atomically(dest, STARTER_CONFIG.as_bytes())
    }
    .map_err(|err| format!("Failed to save {}: {err}", dest.display()))?;

    if let Some((path, registry, _registry_lock)) = registration {
        registry
            .save_to_path(&path)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

// Evita que la configuración apunte al registro y que se intente bloquear la misma ruta dos veces.
fn ensure_distinct_config(registry: &Path, config: &Path) -> Result<(), String> {
    if normalize_absolute_path(registry) == normalize_absolute_path(config) {
        return Err("Workspace config and workspace registry must use different files.".into());
    }
    Ok(())
}

// Muestra el fallo de bloqueo antes de salir de un comando que modifica un TOML.
fn lock_cli_update(path: &Path) -> Result<File, ()> {
    lock_config_update(path).map_err(|err| eprintln!("Failed to lock {}: {err}", path.display()))
}

// Resolve an explicit selection by service name or, for renamed services, the folder name.
fn filter_selected_repos(
    discovered: &[DiscoveredRepo],
    select: &[String],
) -> Result<Vec<DiscoveredRepo>, String> {
    let mut selected = Vec::new();
    for raw in select {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let mut matched: Vec<_> = discovered.iter().filter(|repo| repo.name == name).collect();
        if matched.is_empty() {
            matched.extend(
                discovered
                    .iter()
                    .filter(|repo| repo.path.file_name().is_some_and(|folder| folder == name)),
            );
        }
        match matched.as_slice() {
            [] => {
                let available: Vec<&str> = discovered.iter().map(|r| r.name.as_str()).collect();
                return Err(format!(
                    "Repository '{name}' was not found in scan results (available: {})",
                    available.join(", ")
                ));
            }
            [repo] => {
                if !selected
                    .iter()
                    .any(|r: &DiscoveredRepo| same_repository_path(&r.path, &repo.path))
                {
                    selected.push((*repo).clone());
                }
            }
            _ => return Err(format!("Repository name '{name}' matches multiple paths.")),
        }
    }
    Ok(selected)
}

// Imprime el resultado del análisis o un error estructurado para el formato solicitado.
fn execute_analysis(req: AnalyzeRequest, output: AnalysisOutput) -> ExitCode {
    let started = Instant::now();
    match run_analysis_with_consumer_revision(
        &req,
        output.no_cache,
        output.include_repo_roots && matches!(output.format, OutputFormat::Json),
        output.consumer_revision,
    ) {
        Ok(mut report) => {
            if output.no_snippets {
                omit_snippets(&mut report);
            }
            let elapsed_ms = started.elapsed().as_millis();
            let exit_code = report.exit_code.unwrap_or(1);

            match output.format {
                OutputFormat::Terminal => {
                    print!("{}", render_terminal(&report, elapsed_ms));
                }
                OutputFormat::Json => match render_json(&report) {
                    Ok(json) => println!("{json}"),
                    Err(_) => {
                        print_json_error(
                            1,
                            "report_serialization_failed",
                            "verify_laine_installation",
                            "Analysis succeeded but JSON could not be generated.",
                        );
                        return ExitCode::FAILURE;
                    }
                },
                OutputFormat::Markdown => {
                    print!("{}", render_markdown(&report));
                }
            }

            ExitCode::from(exit_code)
        }
        Err(err) => {
            print_analysis_error("Analysis", &err, output);
            ExitCode::FAILURE
        }
    }
}

// Resolve the service, run the endpoint lookup, and print the requested report format.
fn execute_consumers(args: ConsumersArgs) -> ExitCode {
    let output = AnalysisOutput {
        format: args.format,
        no_cache: args.no_cache,
        no_snippets: args.no_snippets,
        include_repo_roots: args.include_repo_roots,
        consumer_revision: args.consumer_revision,
    };
    let result = resolve_command_context(
        args.config.as_deref(),
        args.workspace.as_deref(),
        args.service.as_deref(),
    )
    .and_then(|context| {
        query_endpoint_consumers(
            &ConsumersRequest {
                config_path: context.config_path,
                service: context.repo_name,
                method: args.method,
                path: args.path,
                field: args.field,
                producer_rev: args.producer_revision,
            },
            output.no_cache,
            output.include_repo_roots && matches!(output.format, OutputFormat::Json),
            output.consumer_revision,
        )
    });
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            print_analysis_error("Consumers", &error, output);
            return ExitCode::FAILURE;
        }
    };
    if output.no_snippets {
        omit_consumers_snippets(&mut report);
    }
    match output.format {
        OutputFormat::Terminal => print!("{}", render_consumers_terminal(&report)),
        OutputFormat::Markdown => print!("{}", render_consumers_markdown(&report)),
        OutputFormat::Json => match render_consumers_json(&report) {
            Ok(json) => println!("{json}"),
            Err(_) => {
                print_json_error(
                    1,
                    "report_serialization_failed",
                    "verify_laine_installation",
                    "The consumers report could not be serialized to JSON.",
                );
                return ExitCode::FAILURE;
            }
        },
    }
    ExitCode::from(report.exit_code)
}

// Translate report failures to stable error codes without including raw input in JSON.
fn print_analysis_error(command: &str, error: &LaineError, output: AnalysisOutput) {
    if matches!(output.format, OutputFormat::Json) {
        let (code, remediation, message) = match error {
            LaineError::Io { .. } => (
                "input_io_error",
                "check_input_paths",
                "An analysis input could not be read. Verify the configuration path and file access.",
            ),
            LaineError::ConfigParse(_) => (
                "invalid_config_syntax",
                "repair_workspace_toml",
                "The workspace TOML could not be parsed.",
            ),
            LaineError::ConfigInvalid(_) if command == "Consumers" => (
                "invalid_workspace_config",
                "check_workspace_selection",
                "Verify the workspace configuration and choose a configured producer with --service.",
            ),
            LaineError::ConfigInvalid(_) => (
                "invalid_workspace_config",
                "check_workspace_selection",
                "Verify the workspace configuration and choose a configured producer with --repo.",
            ),
            LaineError::Git { .. } => (
                "git_snapshot_error",
                "check_git_revisions",
                "Verify that the repositories and Git revisions are available locally.",
            ),
            LaineError::OpenApiParse(_) => (
                "invalid_openapi_document",
                "repair_openapi_document",
                "The producer OpenAPI document could not be analyzed.",
            ),
            LaineError::Cache(_) => (
                "cache_error",
                "retry_without_cache",
                "Retry the analysis with --no-cache or check the cache directory.",
            ),
        };
        print_json_error(1, code, remediation, message);
    } else if output.no_snippets {
        eprintln!("{command} failed. Run locally without --no-snippets for details.");
    } else {
        eprintln!("{command} error: {error}");
    }
}

// Emite un solo documento JSON en stdout para errores de uso o análisis.
fn print_json_error(exit_code: u8, code: &str, remediation: &str, message: &str) {
    println!(
        "{}",
        serde_json::json!({
            "schema_version": 1,
            "status": "error",
            "exit_code": exit_code,
            "diagnostics": [{ "code": code, "remediation": remediation, "message": message }],
        })
    );
}
