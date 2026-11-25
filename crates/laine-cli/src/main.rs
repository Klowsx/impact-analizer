use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use laine_cli::engine::{AnalyzeRequest, index_workspace, run_analysis_with_options};
use laine_cli::report::{render_json, render_markdown, render_terminal};
use laine_core::config::{
    PolicyConfig, RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig,
};
use laine_core::fixtures::create_demo_workspace;
use laine_core::git::STAGED_REV;
use laine_core::policy::evaluate_policy;
use laine_core::workspace::{
    DiscoveredRepo, WorkspaceRegistry, build_workspace_config, resolve_context_from_cwd,
    scan_directory,
};

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
    },
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
    },
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
    },
    /// Unregister a named workspace (does not delete its laine.toml file)
    Remove {
        /// Workspace name to remove
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum RepoAction {
    /// Register a local Git service repository in laine.toml
    Add {
        /// Path to the local Git repository
        path: PathBuf,
        /// Unique service name (defaults to the folder name)
        #[arg(long)]
        name: Option<String>,
        /// Path to laine.toml
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
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

fn main() -> ExitCode {
    let cli = Cli::parse();

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
        Commands::Repo {
            action:
                RepoAction::Add {
                    path,
                    name,
                    config,
                    openapi,
                    base_urls,
                    framework,
                },
        } => {
            let service_name = name.unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "service".to_string())
            });

            let mut cfg = if config.exists() {
                match WorkspaceConfig::load_from_path(&config) {
                    Ok(c) => c,
                    Err(err) => {
                        eprintln!("Failed to load {}: {err}", config.display());
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
                    config.display()
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
                path,
                openapi,
                routes,
                base_urls,
            });

            match cfg.save_to_path(&config) {
                Ok(()) => {
                    println!("Added service '{service_name}' to {}", config.display());
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to save {}: {err}", config.display());
                    ExitCode::FAILURE
                }
            }
        }
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
        } => {
            let cwd = match env::current_dir() {
                Ok(dir) => dir,
                Err(err) => {
                    eprintln!("Failed to determine current directory: {err}");
                    return ExitCode::FAILURE;
                }
            };
            let registry_path = match WorkspaceRegistry::default_path() {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("Workspace registry error: {err}");
                    return ExitCode::FAILURE;
                }
            };
            let resolved = match resolve_context_from_cwd(
                &cwd,
                config.as_deref(),
                workspace.as_deref(),
                repo.as_deref(),
                &registry_path,
            ) {
                Ok(ctx) => ctx,
                Err(err) => {
                    eprintln!("Check error: {err}");
                    return ExitCode::FAILURE;
                }
            };

            let effective_head = if staged { STAGED_REV.to_string() } else { head };
            execute_analysis(
                resolved.config_path,
                resolved.repo_name,
                base,
                effective_head,
                format,
                no_cache,
            )
        }
        Commands::Analyze {
            config,
            repo,
            base,
            head,
            format,
            no_cache,
        } => execute_analysis(config, repo, base, head, format, no_cache),
    }
}

fn handle_init(
    path: Option<PathBuf>,
    force: bool,
    scan: Option<PathBuf>,
    select: Vec<String>,
    all: bool,
    workspace: Option<String>,
) -> ExitCode {
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
            match prompt_select_repos(&discovered) {
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
        let cfg = build_workspace_config(&selected, config_dir, false);
        if let Err(err) = cfg.save_to_path(&dest) {
            eprintln!("Failed to save {}: {err}", dest.display());
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
        if !cfg.clients.is_empty() {
            println!("Auto-detected HTTP client wrapper(s):");
            for client in &cfg.clients {
                let prefix_note = client
                    .path_prefix
                    .as_deref()
                    .map(|p| format!(" (prefix: {p})"))
                    .unwrap_or_default();
                println!(
                    "  - `{}` -> {}{prefix_note}",
                    client.callee, client.target_service
                );
            }
        }
        if !cfg.env.is_empty() {
            println!("Auto-detected {} API URL env var(s).", cfg.env.len());
        }
    } else if let Err(err) = fs::write(&dest, STARTER_CONFIG) {
        eprintln!("Failed to write {}: {err}", dest.display());
        return ExitCode::FAILURE;
    } else {
        println!("Initialized {}", dest.display());
    }

    if let Some(ws_name) = workspace {
        if let Err(msg) = register_workspace(&ws_name, &dest) {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
        println!("Registered workspace '{ws_name}' -> {}", dest.display());
    }

    ExitCode::SUCCESS
}

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
        WorkspaceAction::Add { name, config } => {
            if !config.is_file() {
                eprintln!("Config file does not exist: {}", config.display());
                return ExitCode::FAILURE;
            }
            if let Err(err) = WorkspaceConfig::load_from_path(&config) {
                eprintln!("Invalid Laine config {}: {err}", config.display());
                return ExitCode::FAILURE;
            }
            match register_workspace(&name, &config) {
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
        WorkspaceAction::Remove { name } => {
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

fn register_workspace(name: &str, config_path: &Path) -> Result<(), String> {
    let registry_path =
        WorkspaceRegistry::default_path().map_err(|e| format!("Workspace registry error: {e}"))?;
    let mut reg = WorkspaceRegistry::load_from_path(&registry_path)
        .map_err(|e| format!("Failed to load workspace registry: {e}"))?;
    reg.register(name, config_path.to_path_buf());
    reg.save_to_path(&registry_path)
        .map_err(|e| format!("Failed to save workspace registry: {e}"))
}

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
        if let Some(repo) = discovered.iter().find(|r| r.name == name) {
            if !selected
                .iter()
                .any(|r: &DiscoveredRepo| r.name == repo.name)
            {
                selected.push(repo.clone());
            }
        } else {
            let available: Vec<&str> = discovered.iter().map(|r| r.name.as_str()).collect();
            return Err(format!(
                "Repository '{name}' was not found in scan results (available: {})",
                available.join(", ")
            ));
        }
    }
    Ok(selected)
}

fn prompt_select_repos(discovered: &[DiscoveredRepo]) -> Result<Vec<DiscoveredRepo>, String> {
    println!("Discovered {} Git repository(ies):", discovered.len());
    for (idx, repo) in discovered.iter().enumerate() {
        println!("  [{}] {} ({})", idx + 1, repo.name, repo.detected_kind);
    }
    print!("Select repositories to include (comma-separated numbers/names, or 'all'): ");
    let _ = io::stdout().flush();

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("Failed to read selection: {e}"))?;
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(discovered.to_vec());
    }

    let mut selected = Vec::new();
    for token in trimmed.split(',') {
        let item = token.trim();
        if item.is_empty() {
            continue;
        }
        if let Ok(num) = item.parse::<usize>() {
            if num == 0 || num > discovered.len() {
                return Err(format!(
                    "Selection index {num} is out of range (1..={})",
                    discovered.len()
                ));
            }
            let repo = &discovered[num - 1];
            if !selected
                .iter()
                .any(|r: &DiscoveredRepo| r.name == repo.name)
            {
                selected.push(repo.clone());
            }
        } else if let Some(repo) = discovered.iter().find(|r| r.name == item) {
            if !selected
                .iter()
                .any(|r: &DiscoveredRepo| r.name == repo.name)
            {
                selected.push(repo.clone());
            }
        } else {
            return Err(format!("Unknown repository selection: '{item}'"));
        }
    }
    Ok(selected)
}

fn execute_analysis(
    config: PathBuf,
    repo: String,
    base: String,
    head: String,
    format: OutputFormat,
    no_cache: bool,
) -> ExitCode {
    let started = Instant::now();
    let workspace_cfg = match WorkspaceConfig::load_from_path(&config) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("Analysis error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let req = AnalyzeRequest {
        config_path: config,
        target_repo: repo,
        base_rev: base,
        head_rev: head,
    };

    match run_analysis_with_options(&req, no_cache) {
        Ok(report) => {
            let elapsed_ms = started.elapsed().as_millis();
            let policy_eval = evaluate_policy(&report, &workspace_cfg.policy);

            match format {
                OutputFormat::Terminal => {
                    print!("{}", render_terminal(&report, elapsed_ms));
                }
                OutputFormat::Json => match render_json(&report) {
                    Ok(json) => println!("{json}"),
                    Err(err) => {
                        eprintln!("Failed to serialize JSON report: {err}");
                        return ExitCode::FAILURE;
                    }
                },
                OutputFormat::Markdown => {
                    print!("{}", render_markdown(&report));
                }
            }

            if policy_eval.should_fail {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("Analysis error: {err}");
            ExitCode::FAILURE
        }
    }
}
