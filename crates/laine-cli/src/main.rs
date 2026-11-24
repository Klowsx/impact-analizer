use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use laine_cli::engine::{AnalyzeRequest, index_workspace, run_analysis_with_options};
use laine_cli::report::{render_json, render_markdown, render_terminal};
use laine_core::config::{
    PolicyConfig, RouteFramework, RouteSourceConfig, ServiceConfig, WorkspaceConfig,
};
use laine_core::fixtures::create_demo_workspace;
use laine_core::policy::evaluate_policy;

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
    /// Initialize a starter laine.toml configuration file
    Init {
        /// Destination path for the configuration file
        #[arg(long, default_value = "laine.toml")]
        path: PathBuf,
        /// Overwrite an existing configuration file
        #[arg(long)]
        force: bool,
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
    /// Analyze cross-repository impact between two Git revisions
    Analyze {
        /// Path to laine.toml
        #[arg(long, default_value = "laine.toml")]
        config: PathBuf,
        /// Service repository where the change occurred
        #[arg(long)]
        repo: String,
        /// Base Git revision (before change)
        #[arg(long)]
        base: String,
        /// Head Git revision (after change)
        #[arg(long)]
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
        Commands::Init { path, force } => {
            if path.exists() && !force {
                eprintln!(
                    "Config file {} already exists (use --force to overwrite)",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
            match fs::write(&path, STARTER_CONFIG) {
                Ok(()) => {
                    println!("Initialized {}", path.display());
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("Failed to write {}: {err}", path.display());
                    ExitCode::FAILURE
                }
            }
        }
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
        Commands::Analyze {
            config,
            repo,
            base,
            head,
            format,
            no_cache,
        } => {
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
    }
}
