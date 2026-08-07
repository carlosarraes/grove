use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use treeish::{instance::Instance, llm};

/// Per-worktree dev instances: each git worktree gets its own ports, env, and database.
#[derive(Parser)]
#[command(name = "treeish", version, about, disable_help_subcommand = true)]
struct Cli {
    /// Emit the .treeish.toml schema and worked examples, then exit
    #[arg(long, global = true)]
    llm: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Render config, ensure resources, start services, wait for ready
    Up {
        /// Restart services that are already running
        #[arg(long)]
        fresh: bool,
        /// Permit running in the main worktree, overwriting its real env files
        #[arg(long)]
        allow_main: bool,
    },
    /// Stop this instance's services
    Down {
        /// Also forget this instance's port reservation
        #[arg(long)]
        purge: bool,
    },
    /// Ports, pids, and health for this instance
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Every instance on this machine
    #[command(visible_alias = "list", alias = "instances")]
    Ls,
    /// Run a command with this instance's environment exported
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Tail a service's log
    Logs {
        service: Option<String>,
        #[arg(short, long)]
        follow: bool,
    },
    /// Check that this worktree can start: env, resources, ports, config
    Doctor,
    /// Manage the agent-facing skill
    Skill {
        #[command(subcommand)]
        action: SkillAction,
    },
}

#[derive(Subcommand)]
enum SkillAction {
    /// Install the skill to ~/.claude/skills/treeish/
    Install,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.llm {
        print!("{}", llm::reference());
        return Ok(());
    }

    let cwd = std::env::current_dir().context("reading the working directory")?;

    match cli.command {
        None => {
            <Cli as clap::CommandFactory>::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Command::Up { fresh, allow_main }) => {
            let mut instance = Instance::open(&cwd)?;
            if !allow_main {
                instance.refuse_in_main()?;
            }
            for started in instance.resources()? {
                eprintln!("started shared {started}");
            }
            instance.render()?;
            instance.up(fresh)?;
            print_summary(&instance);
            Ok(())
        }
        Some(Command::Down { purge }) => {
            let mut instance = Instance::open(&cwd)?;
            instance.down()?;
            if purge {
                // Report a failed drop without failing `down` — the services really did
                // stop, and a leftover database is a smaller problem than a command that
                // looks like it did nothing.
                if let Err(e) = instance.purge_database() {
                    eprintln!("{e:#}");
                }
                instance.release()?;
            }
            println!("stopped {}", instance.resolved.slug);
            Ok(())
        }
        Some(Command::Status { json }) => {
            let instance = Instance::open(&cwd)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status_json(&instance))?);
            } else {
                print_summary(&instance);
            }
            Ok(())
        }
        Some(Command::Ls) => {
            let registry = treeish::instance::registry()?;
            registry.reap()?;
            let entries = registry.list()?;
            if entries.is_empty() {
                println!("no instances");
            }
            for entry in entries {
                // Ask the processes, not the record. A recorded pid says only that a
                // service was started once; reading that as "up" turns a stopped
                // instance into an apparent port conflict.
                let live = entry
                    .services
                    .values()
                    .filter(|h| treeish::supervise::is_alive(h))
                    .count();
                let state = if live > 0 {
                    format!("running ({live})")
                } else {
                    "stopped".to_string()
                };
                let ports: Vec<String> = entry
                    .ports
                    .iter()
                    .map(|(n, p)| format!("{n}={p}"))
                    .collect();
                println!("{:<28} {:<13} {}", entry.slug, state, ports.join(" "));
            }
            Ok(())
        }
        Some(Command::Run { argv }) => {
            let instance = Instance::open(&cwd)?;
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&cwd)
                .envs(instance.environment())
                .status()
                .with_context(|| format!("running `{}`", argv.join(" ")))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Some(Command::Logs { service, follow }) => {
            let instance = Instance::open(&cwd)?;
            let name = match service {
                Some(name) => name,
                None => instance
                    .config
                    .services
                    .first()
                    .map(|s| s.name.clone())
                    .context("this repo declares no services")?,
            };
            let path = instance.log_path(&name);
            if follow {
                let status = std::process::Command::new("tail")
                    .arg("-f")
                    .arg(&path)
                    .status()
                    .context("running tail")?;
                std::process::exit(status.code().unwrap_or(0));
            }
            match std::fs::read_to_string(&path) {
                Ok(body) => print!("{body}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    bail!("{name} has no log yet at {}", path.display())
                }
                Err(e) => return Err(e).context("reading the log"),
            }
            Ok(())
        }
        Some(Command::Doctor) => {
            let instance = Instance::open(&cwd)?;
            let verdicts = treeish::doctor::check(&instance);
            if treeish::doctor::report(&verdicts)? {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::Skill {
            action: SkillAction::Install,
        }) => {
            let path = treeish::skill::install()?;
            println!("installed the treeish skill to {}", path.display());
            Ok(())
        }
    }
}

fn status_json(instance: &Instance) -> serde_json::Value {
    let services: serde_json::Map<String, serde_json::Value> = instance
        .status()
        .into_iter()
        .map(|s| (s.name, serde_json::json!({ "running": s.running })))
        .collect();
    serde_json::json!({
        "slug": instance.resolved.slug,
        "worktree": instance.resolved.worktree,
        "ports": instance.entry.ports,
        "database": instance.db_name(),
        "services": services,
    })
}

/// The only thing an agent should need to read after `up`.
fn print_summary(instance: &Instance) {
    println!("instance  {}", instance.resolved.slug);
    for (name, port) in &instance.entry.ports {
        println!("{name:<10}http://localhost:{port}");
    }
    if let Some(db) = instance.db_name() {
        println!("database  {db}");
    }
    println!();
    let first = instance
        .config
        .services
        .first()
        .map(|s| s.name.as_str())
        .unwrap_or("<service>");
    println!("  treeish run -- <your test command>");
    println!("  treeish logs {first} -f");
}
