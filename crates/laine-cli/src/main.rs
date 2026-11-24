use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use laine_cli::engine::{AnalyzeRequest, run_analysis};
use laine_cli::report::{render_json, render_terminal};
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
        } => {
            let started = Instant::now();
            let req = AnalyzeRequest {
                config_path: config,
                target_repo: repo,
                base_rev: base,
                head_rev: head,
            };

            match run_analysis(&req) {
                Ok(report) => {
                    let elapsed_ms = started.elapsed().as_millis();
                    let has_breaking_changes = !report.findings.is_empty();

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
                    }

                    if has_breaking_changes {
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
