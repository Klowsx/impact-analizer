use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use laine_core::config::WorkspaceConfig;
use laine_core::fixtures::create_demo_workspace;

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
    /// Materialize the reproducible 4-repository Git demo workspace
    DemoSetup {
        /// Target directory for the generated demo workspace
        #[arg(default_value = "fixtures/demo-workspace")]
        path: PathBuf,
    },
    /// Analyze cross-repository impact between two Git revisions (implemented in Phase 1)
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
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Terminal,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
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
        Commands::Analyze {
            config,
            repo,
            base,
            head,
            format,
        } => match WorkspaceConfig::load_from_path(&config) {
            Ok(cfg) => {
                if cfg.find_service(&repo).is_none() {
                    eprintln!("Service '{repo}' not found in {}", config.display());
                    return ExitCode::FAILURE;
                }
                println!(
                    "Loaded config ({} services). Analysis engine for {repo} ({base} -> {head}, format: {format:?}) will be wired in Phase 1.",
                    cfg.services.len()
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Failed to load config: {err}");
                ExitCode::FAILURE
            }
        },
    }
}
