use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "sequencer-sync")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Setup(CommandArgs),
    Test(CommandArgs),
    Run(CommandArgs),
}

#[derive(Args, Debug)]
struct CommandArgs {
    #[arg(long)]
    config_path: PathBuf,
    #[arg(long)]
    platform: Platform,
}

#[derive(Clone, Debug, ValueEnum)]
enum Platform {
    Nanopore,
    NextSeq,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Setup(args) => setup(args),
        Commands::Test(args) => test(args),
        Commands::Run(args) => run_command(args),
    }
}

fn setup(args: CommandArgs) -> ExitCode {
    match args.platform {
        Platform::Nanopore => setup_nanopore(&args.config_path),
        Platform::NextSeq => setup_next_seq(&args.config_path),
    }
}

fn test(args: CommandArgs) -> ExitCode {
    match args.platform {
        Platform::Nanopore => test_nanopore(&args.config_path),
        Platform::NextSeq => test_next_seq(&args.config_path),
    }
}

fn run_command(args: CommandArgs) -> ExitCode {
    match args.platform {
        Platform::Nanopore => run_nanopore(&args.config_path),
        Platform::NextSeq => run_next_seq(&args.config_path),
    }
}

fn setup_nanopore(_config_path: &Path) -> ExitCode {
    todo!()
}

fn setup_next_seq(_config_path: &Path) -> ExitCode {
    todo!()
}

fn test_nanopore(_config_path: &Path) -> ExitCode {
    todo!()
}

fn test_next_seq(_config_path: &Path) -> ExitCode {
    todo!()
}

fn run_nanopore(_config_path: &Path) -> ExitCode {
    todo!()
}

fn run_next_seq(_config_path: &Path) -> ExitCode {
    todo!()
}
