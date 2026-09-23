//! jjn — CLI for jjunction.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clap::Subcommand;
use jj_lib::config::StackedConfig;
use jjunction::config::global::GlobalConfigReader;
use jjunction::config::local::LocalConfigReader;
use jjunction::config::trust::is_trusted;
use jjunction::find_workspace_root;
use jjunction::link;
use jjunction::workspace;

#[derive(Parser)]
#[command(name = "jjn", version, about = "Jujutsu workspace tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the configured [[link]] entries and sync workspaces
    Apply {
        /// Replace links that exist but point elsewhere (never real directories)
        #[arg(long)]
        force: bool,
        /// Suppress informational output
        #[arg(long, short = 'q')]
        quiet: bool,
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Check configuration and [[link]] entry health
    Doctor {
        #[command(flatten)]
        common: CommonArgs,
    },
}

#[derive(clap::Args)]
struct CommonArgs {
    /// Workspace root to operate on (searched upwards from this path)
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Path to the global config file
    #[arg(long)]
    global_config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Apply {
            force,
            quiet,
            common,
        } => cmd_apply(common, force, quiet),
        Command::Doctor { common } => cmd_doctor(common),
    }
}

struct Loaded {
    root: PathBuf,
    global_path: PathBuf,
    global: StackedConfig,
    local: StackedConfig,
}

fn load(common: &CommonArgs) -> Result<Loaded, String> {
    let root = find_workspace_root(&common.root).ok_or_else(|| {
        format!(
            "no workspace root found at or above {}",
            common.root.display()
        )
    })?;
    let global_path = match &common.global_config {
        Some(path) => path.clone(),
        None => GlobalConfigReader::default_path()
            .ok_or_else(|| "no platform config directory on this system".to_owned())?,
    };

    let mut global = StackedConfig::empty();
    if let Some(layer) = GlobalConfigReader::from_path(&global_path)
        .layer()
        .map_err(|err| format!("global config: {err}"))?
    {
        global.add_layer(layer);
    }
    let mut local = StackedConfig::empty();
    if let Some(layer) = LocalConfigReader::new(&root)
        .layer()
        .map_err(|err| format!("local config: {err}"))?
    {
        local.add_layer(layer);
    }
    Ok(Loaded {
        root,
        global_path,
        global,
        local,
    })
}

fn cmd_apply(common: CommonArgs, force: bool, quiet: bool) -> ExitCode {
    let loaded = match load(&common) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let entries = match link::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[link]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    if entries.is_empty() && !quiet {
        println!("no [[link]] entries");
    }
    if !is_trusted(&loaded.global, &loaded.root) {
        if !entries.is_empty() {
            println!(
                "repo {} is not trusted; add it to trusted-repos in {} to apply links",
                loaded.root.display(),
                loaded.global_path.display()
            );
        }
        return ExitCode::SUCCESS;
    }

    let mut failed = false;
    for entry in &entries {
        match link::apply_one(&loaded.root, entry, force) {
            Ok(status) => {
                if !quiet {
                    println!("{}: {status}", entry.target)
                }
            }
            Err(err) => {
                eprintln!("{}: {err}", entry.target);
                failed = true;
            }
        }
    }

    // Phase 2: cross-workspace sync (main → others).
    match workspace::list(&loaded.root) {
        Ok(all) if all.len() > 1 => {
            let Some(default) = all.iter().find(|ws| ws.is_default) else {
                eprintln!("warning: workspace sync skipped: no default workspace found");
                return exit_code(failed);
            };
            let others: Vec<_> = all
                .iter()
                .filter(|ws| !ws.is_default)
                .map(|ws| ws.root.clone())
                .collect();
            let ws_config = match jjunction::config::load_workspace_config(&loaded.local) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("warning: workspace sync skipped: {err}");
                    return exit_code(failed);
                }
            };
            let reports = workspace::sync_workspaces(&default.root, &others, &ws_config, force);
            if !quiet {
                for report in &reports {
                    for (name, status) in &report.files {
                        println!("{} [{name}]: {status}", report.root.display());
                    }
                    println!("{} [allow]: {}", report.root.display(), report.allow);
                }
            }
        }
        Ok(_) => {} // single workspace: nothing to sync
        Err(err) => {
            eprintln!("warning: workspace sync skipped: {err}");
        }
    }

    exit_code(failed)
}

fn exit_code(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_doctor(common: CommonArgs) -> ExitCode {
    let loaded = match load(&common) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let entries = match link::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[link]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    if entries.is_empty() {
        println!("no [[link]] entries; nothing to check");
        return ExitCode::SUCCESS;
    }
    if !is_trusted(&loaded.global, &loaded.root) {
        println!(
            "untrusted: repo {} is not in trusted-repos ({}); {} link entries skipped",
            loaded.root.display(),
            loaded.global_path.display(),
            entries.len()
        );
        return ExitCode::SUCCESS;
    }

    let mut problems = 0;
    for diagnosis in link::doctor(&loaded.root, &entries) {
        let mark = if diagnosis.status.is_ok() {
            "ok"
        } else {
            "FAIL"
        };
        println!(
            "{} [{}]: {}",
            diagnosis.entry.target, mark, diagnosis.status
        );
        if !diagnosis.status.is_ok() {
            problems += 1;
        }
    }
    if problems == 0 {
        ExitCode::SUCCESS
    } else {
        eprintln!("{problems} problem(s) found");
        ExitCode::FAILURE
    }
}
