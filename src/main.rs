use anyhow::Result;
use clap::{Parser, Subcommand};
use treeish::llm;

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
        /// Re-render config and restart even if the instance is already running
        #[arg(long)]
        fresh: bool,
        /// Permit running in the main worktree, overwriting its real env files
        #[arg(long)]
        allow_main: bool,
    },
    /// Stop this instance's services
    Down {
        /// Also drop this instance's database
        #[arg(long)]
        purge: bool,
    },
    /// Ports, pids, and health for this instance
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Every instance on this machine
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

    match cli.command {
        None => {
            <Cli as clap::CommandFactory>::command().print_help()?;
            println!();
            Ok(())
        }
        Some(_) => anyhow::bail!("not implemented yet"),
    }
}
