use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use config::Config;

mod config;

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
    let config = load_config(&args.config_path);

    // TODO: Figure out error propagation now.
    if let Err(error) = check_ssh_access(&config) {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }

    if let Err(error) = check_remote_write_access(&config) {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }

    // TODO later: Check that landing zone exists, and you have write permissions
    // TODO later: Check that source exists, and you have read permissions
    // TODO later: Check the flock dir exists, and you have write permissions

    ExitCode::SUCCESS
}

fn test(args: CommandArgs) -> ExitCode {
    match args.platform {
        Platform::Nanopore => test_nanopore(&args.config_path),
        Platform::NextSeq => test_nextseq(&args.config_path),
    }
}

fn run_command(args: CommandArgs) -> ExitCode {
    match args.platform {
        Platform::Nanopore => run_nanopore(&args.config_path),
        Platform::NextSeq => run_nextseq(&args.config_path),
    }
}

fn test_nanopore(_config_path: &Path) -> ExitCode {
    let _config = load_config(_config_path);
    todo!()
}

fn test_nextseq(_config_path: &Path) -> ExitCode {
    let _config = load_config(_config_path);
    todo!()
}

fn run_nanopore(_config_path: &Path) -> ExitCode {
    let _config = load_config(_config_path);
    todo!()
}

fn run_nextseq(_config_path: &Path) -> ExitCode {
    let _config = load_config(_config_path);
    todo!()
}

fn load_config(config_path: &Path) -> Config {
    Config::from_path(config_path).unwrap_or_else(|error| {
        panic!(
            "failed to load config from {}: {error}",
            config_path.display()
        )
    })
}

fn check_ssh_access(config: &Config) -> Result<(), String> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(config.server_port.to_string())
        .arg(format!("{}@{}", config.server_user, config.server_host))
        .arg("--")
        .arg("true")
        .status()
        .map_err(|error| format!("failed to execute ssh access check: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "ssh access check failed for {}@{}:{}",
            config.server_user, config.server_host, config.server_port
        ))
    }
}

fn check_remote_write_access(config: &Config) -> Result<(), String> {
    let status = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg(config.server_port.to_string())
        .arg(format!("{}@{}", config.server_user, config.server_host))
        .arg("test")
        .arg("-w")
        .arg(&config.server_dest)
        .status()
        .map_err(|error| format!("failed to execute remote write-access check: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "remote write-access check failed for {} on {}@{}:{}",
            config.server_dest.display(),
            config.server_user,
            config.server_host,
            config.server_port
        ))
    }
}
