//! jjn — CLI for jjunction.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clap::Subcommand;
use jj_lib::config::StackedConfig;
use jjunction::config::LOCAL_CONFIG_DIR;
use jjunction::config::LOCAL_CONFIG_FILE;
use jjunction::config::LOCAL_LOCK_FILE;
use jjunction::config::global::GlobalConfigReader;
use jjunction::config::local::LocalConfigReader;
use jjunction::config::trust::is_trusted;
use jjunction::find_workspace_root;
use jjunction::hooks;
use jjunction::link;
use jjunction::lock::RepoLock;
use jjunction::repo;
use jjunction::workspace;

#[derive(Parser)]
#[command(name = "jjn", version, about = "Jujutsu workspace tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Materialize [[repo]] entries, create [[link]] entries, and sync workspaces
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
    /// Check configuration, [[repo]], and [[link]] entry health
    Doctor {
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Manage [[repo]] sub-repo entries
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Wire the workspace reaction loop (.envrc watch block, devenv hook)
    Init {
        #[command(flatten)]
        common: CommonArgs,
    },
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Add a sub-repo to the manifest and materialize it
    Add {
        /// Clone url of the sub-repo
        url: String,
        /// Entry name (default: url basename minus .git)
        #[arg(long)]
        name: Option<String>,
        /// Materialization path, relative to the workspace root (default: name)
        #[arg(long)]
        target: Option<String>,
        /// Branch, tag, or full commit id (default: follow the default branch)
        #[arg(long)]
        rev: Option<String>,
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Remove a sub-repo from the manifest (files stay on disk)
    Remove {
        /// Entry name or target path
        name: String,
        #[command(flatten)]
        common: CommonArgs,
    },
    /// Fetch remotes, re-resolve revisions, and advance the lock
    Update {
        /// Entry names to update (default: all)
        names: Vec<String>,
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
        Command::Repo { command } => cmd_repo(command),
        Command::Init { common } => cmd_init(common),
    }
}

struct Loaded {
    root: PathBuf,
    global_path: PathBuf,
    global: StackedConfig,
    local: StackedConfig,
}

impl Loaded {
    fn lock_path(&self) -> PathBuf {
        self.root.join(LOCAL_CONFIG_DIR).join(LOCAL_LOCK_FILE)
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join(LOCAL_CONFIG_DIR).join(LOCAL_CONFIG_FILE)
    }
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

    let repos = match repo::load_entries(&loaded.local) {
        Ok(repos) => repos,
        Err(err) => {
            eprintln!("error: local config [[repo]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = repo::validate(&repos) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }
    let entries = match link::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[link]]: {err}");
            return ExitCode::FAILURE;
        }
    };

    if !is_trusted(&loaded.global, &loaded.root) {
        if (!repos.is_empty() || !entries.is_empty()) && !quiet {
            println!(
                "repo {} is not trusted; add it to trusted-repos in {} to apply [[repo]] and [[link]] entries",
                loaded.root.display(),
                loaded.global_path.display()
            );
        }
        return ExitCode::SUCCESS;
    }

    let mut failed = false;

    // Phase 1: sub-repos. Links may point into materialized repos, so repos
    // come first.
    if !repos.is_empty() {
        let mut lock = match RepoLock::load_or_create(&loaded.lock_path()) {
            Ok(lock) => lock,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };
        for entry in &repos {
            match repo::apply_one(&loaded.root, entry, &mut lock, false) {
                Ok(status) => {
                    if !quiet {
                        println!(
                            "{} ({}): {status}",
                            entry.effective_name(),
                            entry.effective_target()
                        );
                    }
                }
                Err(err) => {
                    eprintln!("{}: {err}", entry.effective_name());
                    failed = true;
                }
            }
        }
        if lock.is_changed()
            && let Err(err) = lock.save()
        {
            eprintln!("error: lock: {err}");
            failed = true;
        }
    }

    // Phase 2: links.
    if entries.is_empty() {
        if !quiet {
            println!("no [[link]] entries");
        }
    } else {
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
    }

    // Phase 3: cross-workspace sync (main → others).
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

    let repos = match repo::load_entries(&loaded.local) {
        Ok(repos) => repos,
        Err(err) => {
            eprintln!("error: local config [[repo]]: {err}");
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

    let mut problems = 0;
    let hooks_health = hooks::doctor(&loaded.root);
    if !hooks_health.is_ok() {
        println!("hooks: FAIL {}", hooks_health);
        problems += 1;
    }

    if repos.is_empty() && entries.is_empty() {
        if problems == 0 {
            println!("no [[repo]] or [[link]] entries; nothing to check");
        }
        return exit_code(problems > 0);
    }
    if !is_trusted(&loaded.global, &loaded.root) {
        println!(
            "untrusted: repo {} is not in trusted-repos ({}); {} repo and {} link entries skipped",
            loaded.root.display(),
            loaded.global_path.display(),
            repos.len(),
            entries.len()
        );
        return exit_code(problems > 0);
    }

    if !repos.is_empty() {
        let lock = match RepoLock::load_or_create(&loaded.lock_path()) {
            Ok(lock) => lock,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };
        for diagnosis in repo::doctor(&loaded.root, &repos, &lock) {
            let mark = if diagnosis.status.is_ok() {
                "ok"
            } else {
                "FAIL"
            };
            println!(
                "{} ({}): {mark} {}",
                diagnosis.entry.effective_name(),
                diagnosis.entry.effective_target(),
                diagnosis.status
            );
            if !diagnosis.status.is_ok() {
                problems += 1;
            }
        }
    }

    if !entries.is_empty() {
        for diagnosis in link::doctor(&loaded.root, &entries) {
            let mark = if diagnosis.status.is_ok() {
                "ok"
            } else {
                "FAIL"
            };
            println!("{}: {mark} {}", diagnosis.entry.target, diagnosis.status);
            if !diagnosis.status.is_ok() {
                problems += 1;
            }
        }
    }

    if problems == 0 {
        ExitCode::SUCCESS
    } else {
        eprintln!("{problems} problem(s) found");
        ExitCode::FAILURE
    }
}

fn cmd_init(common: CommonArgs) -> ExitCode {
    let root = match find_workspace_root(&common.root) {
        Some(root) => root,
        None => {
            eprintln!(
                "error: no workspace root found at or above {}",
                common.root.display()
            );
            return ExitCode::FAILURE;
        }
    };
    match hooks::init(&root) {
        Ok(report) => {
            println!(".jjunction/config.toml: {}", report.config);
            println!(".envrc: {}", report.envrc);
            println!("devenv.local.nix: {}", report.devenv_local);
            if report.devenv_local == hooks::WireStatus::Skipped {
                println!("  (no devenv.yaml/devenv.nix: not a devenv project)");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_repo(command: RepoCommand) -> ExitCode {
    match command {
        RepoCommand::Add {
            url,
            name,
            target,
            rev,
            common,
        } => cmd_repo_add(
            common,
            repo::RepoEntry {
                name,
                url,
                target,
                rev,
            },
        ),
        RepoCommand::Remove { name, common } => cmd_repo_remove(common, name),
        RepoCommand::Update { names, common } => cmd_repo_update(common, names),
    }
}

/// Explicit manifest edits are user intent, so they need no trust gate; the
/// trust gate governs `apply`/`doctor` acting on untrusted config.
fn cmd_repo_add(common: CommonArgs, entry: repo::RepoEntry) -> ExitCode {
    let loaded = match load(&common) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let existing = match repo::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[repo]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut merged = existing.clone();
    merged.push(entry.clone());
    if let Err(err) = repo::validate(&merged) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }

    if let Err(err) = repo::append_manifest_entry(&loaded.manifest_path(), &entry) {
        eprintln!("error: manifest: {err}");
        return ExitCode::FAILURE;
    }
    let name = entry.effective_name();
    println!(
        "added {name} ({}) to {}",
        entry.effective_target(),
        loaded.manifest_path().display()
    );

    let mut lock = match RepoLock::load_or_create(&loaded.lock_path()) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let failed = match repo::apply_one(&loaded.root, &entry, &mut lock, false) {
        Ok(status) => {
            println!("{name}: {status}");
            false
        }
        Err(err) => {
            eprintln!("{name}: {err}");
            true
        }
    };
    if lock.is_changed()
        && let Err(err) = lock.save()
    {
        eprintln!("error: lock: {err}");
        return ExitCode::FAILURE;
    }
    exit_code(failed)
}

fn cmd_repo_remove(common: CommonArgs, name: String) -> ExitCode {
    let loaded = match load(&common) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let entries = match repo::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[repo]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(found) = entries
        .iter()
        .find(|entry| entry.effective_name() == name || entry.effective_target() == name)
    else {
        eprintln!(
            "error: no [[repo]] entry named {name}; known: {}",
            entries
                .iter()
                .map(repo::RepoEntry::effective_name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::FAILURE;
    };
    let effective_name = found.effective_name();

    match repo::remove_manifest_entry(&loaded.manifest_path(), &effective_name) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("error: {name} disappeared from the manifest while removing it");
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("error: manifest: {err}");
            return ExitCode::FAILURE;
        }
    }
    let mut lock = match RepoLock::load_or_create(&loaded.lock_path()) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    lock.remove(&effective_name);
    if lock.is_changed()
        && let Err(err) = lock.save()
    {
        eprintln!("error: lock: {err}");
        return ExitCode::FAILURE;
    }
    println!(
        "removed {effective_name}; directory left at {} (delete it manually if desired)",
        found.effective_target()
    );
    ExitCode::SUCCESS
}

fn cmd_repo_update(common: CommonArgs, names: Vec<String>) -> ExitCode {
    let loaded = match load(&common) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let entries = match repo::load_entries(&loaded.local) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: local config [[repo]]: {err}");
            return ExitCode::FAILURE;
        }
    };
    if entries.is_empty() {
        eprintln!("error: no [[repo]] entries");
        return ExitCode::FAILURE;
    }

    let selected: Vec<_> = if names.is_empty() {
        entries
    } else {
        let mut selected = Vec::new();
        for name in &names {
            let Some(found) = entries
                .iter()
                .find(|entry| entry.effective_name() == *name || entry.effective_target() == *name)
            else {
                eprintln!(
                    "error: no [[repo]] entry named {name}; known: {}",
                    entries
                        .iter()
                        .map(repo::RepoEntry::effective_name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                return ExitCode::FAILURE;
            };
            selected.push(found.clone());
        }
        selected
    };

    let mut lock = match RepoLock::load_or_create(&loaded.lock_path()) {
        Ok(lock) => lock,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    for entry in &selected {
        match repo::apply_one(&loaded.root, entry, &mut lock, true) {
            Ok(status) => {
                println!(
                    "{} ({}): {status}",
                    entry.effective_name(),
                    entry.effective_target()
                );
            }
            Err(err) => {
                eprintln!("{}: {err}", entry.effective_name());
                failed = true;
            }
        }
    }
    if lock.is_changed()
        && let Err(err) = lock.save()
    {
        eprintln!("error: lock: {err}");
        failed = true;
    }
    exit_code(failed)
}
