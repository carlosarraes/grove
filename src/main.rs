use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grove::registry::{Entry, human_age};
use grove::{instance::Instance, llm, load};
use std::path::Path;

/// The window the printed prescription proposes. Generous on purpose: an instance someone
/// stepped away from for lunch has to survive it, because whoever runs the command cannot
/// tell a forgotten box from a colleague's.
const IDLE_WINDOW: &str = "2h";

/// What this machine looks like right now — how loaded, how crowded, and which instances
/// nobody has touched lately.
struct Machine {
    load: Option<load::Load>,
    running: usize,
    /// Slug and idle age, oldest first, for instances past `IDLE_WINDOW`.
    idle: Vec<(String, u64)>,
}

/// `exclude` is the worktree the caller is standing in, which `down --idle` will not stop
/// and so must not be offered.
fn survey(exclude: Option<&Path>) -> Result<Machine> {
    let now = grove::registry::now();
    let window = grove::instance::parse_duration(IDLE_WINDOW)?.as_secs();

    let running: Vec<Entry> = grove::instance::registry()?
        .list()?
        .into_iter()
        .filter(Entry::is_running)
        .collect();

    let mut idle: Vec<(String, u64)> = running
        .iter()
        .filter(|e| exclude.is_none_or(|w| e.worktree != w))
        .filter_map(|e| Some((e.slug.clone(), e.idle_seconds(now)?)))
        .filter(|(_, age)| *age >= window)
        .collect();
    idle.sort_by_key(|(_, age)| std::cmp::Reverse(*age));

    Ok(Machine {
        load: load::sample(),
        running: running.len(),
        idle,
    })
}

impl Machine {
    fn crowded(&self) -> bool {
        load::should_warn(self.load.as_ref(), self.running)
    }

    fn headline(&self) -> String {
        match &self.load {
            Some(l) => format!(
                "load {:.1} on {} cores, {} instances running",
                l.one, l.cores, self.running
            ),
            None => format!("{} instances running", self.running),
        }
    }

    /// Names, not a count: "would stop 7" answers *how many* when the question is *which*,
    /// and on a shared machine the names are the blast radius.
    ///
    /// None when nothing is stale — a prescription that would stop nothing teaches the
    /// reader that grove's prescriptions are noise.
    fn prescription(&self) -> Option<Vec<String>> {
        const SHOWN: usize = 6;
        if self.idle.is_empty() {
            return None;
        }
        let mut named: Vec<String> = self
            .idle
            .iter()
            .take(SHOWN)
            .map(|(slug, age)| format!("{slug} ({})", human_age(*age)))
            .collect();
        if let Some(rest) = self.idle.len().checked_sub(SHOWN).filter(|n| *n > 0) {
            named.push(format!("and {rest} more"));
        }
        Some(vec![
            format!(
                "  {} idle over {IDLE_WINDOW}: {}",
                self.idle.len(),
                named.join(", ")
            ),
            format!("  grove down --idle {IDLE_WINDOW}   stops those, keeping their ports"),
        ])
    }
}

/// Per-worktree dev instances: each git worktree gets its own ports, env, and database.
#[derive(Parser)]
#[command(name = "grove", version, about, disable_help_subcommand = true)]
struct Cli {
    /// Emit the .grove.toml schema and worked examples, then exit
    #[arg(long, global = true)]
    llm: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Render config, start missing shared containers with nofile=64000, start services
    Up {
        /// Restart services that are already running
        #[arg(long)]
        fresh: bool,
        /// Permit running in the main worktree, overwriting its real env files
        #[arg(long)]
        allow_main: bool,
        /// Expose opted-in services to the local network using the default-route IPv4
        #[arg(long)]
        expose: bool,
        /// Expose to the local network using this IPv4 address or hostname
        #[arg(long, value_name = "HOST")]
        expose_host: Option<String>,
    },
    /// Stop this instance's services
    Down {
        /// Also forget this instance's port reservation
        #[arg(long)]
        purge: bool,
        /// Instead: stop every instance nobody has worked in for this long (e.g. 2h)
        #[arg(long, value_name = "DURATION")]
        idle: Option<String>,
        /// Instead: stop every running instance except this one
        #[arg(long)]
        all_but_this: bool,
        /// Name what would be stopped, and stop nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Ports, pids, and whether each service's endpoint actually answers
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Every instance on this machine
    #[command(visible_alias = "list", alias = "instances")]
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Run a command with this instance's environment overlaid
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Tail a service's log
    Logs {
        service: Option<String>,
        #[arg(short, long)]
        follow: bool,
        /// Show only the last N lines
        #[arg(short = 'n', long)]
        lines: Option<usize>,
        /// Show only what this service printed since it last started
        #[arg(long)]
        since_restart: bool,
    },
    /// Stop a service and start it again; all of them if none is named
    Restart { service: Option<String> },
    /// Stop and forget instances whose worktree no longer exists
    Prune {
        /// Also drop their databases
        #[arg(long)]
        purge: bool,
        /// Name what would be reclaimed, and reclaim nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Populate data; rerun after seed changes or resource recreation
    Seed {
        /// Re-run seeds that already ran for this instance
        #[arg(long)]
        force: bool,
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
    /// Install the skill to ~/.claude/skills/grove/
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
        Some(Command::Up {
            fresh,
            allow_main,
            expose,
            expose_host,
        }) => {
            let exposure = match expose_host {
                Some(host) => grove::exposure::Exposure::explicit(&host)?,
                None if expose => grove::exposure::Exposure::detect()?,
                None => grove::exposure::Exposure::local(),
            };
            let mut instance = Instance::open(&cwd)?;
            if !allow_main {
                instance.refuse_in_main()?;
            }
            instance.touch()?;

            if exposure.is_exposed() {
                eprintln!(
                    "warning: exposing {} on all interfaces as {}; development services and authentication bypasses may be reachable from other machines",
                    instance.resolved.slug,
                    exposure.public_host()
                );
            }

            // Before starting, not after: the pile-up forms one agent at a time, and this
            // is the only moment the one adding to it is paying attention.
            let machine = survey(Some(&instance.resolved.worktree))?;
            if machine.crowded() {
                eprintln!("warning: {}", machine.headline());
                for line in machine.prescription().unwrap_or_default() {
                    eprintln!("{line}");
                }
            }

            for started in instance.resources()? {
                eprintln!("started shared {started}");
            }
            instance.render_for_up(exposure)?;
            for outcome in instance.up(fresh)? {
                eprintln!("{outcome}");
            }
            print_summary(&instance);
            Ok(())
        }
        Some(Command::Down {
            purge,
            idle,
            all_but_this,
            dry_run,
        }) => {
            if idle.is_some() || all_but_this {
                let instance = Instance::open(&cwd)?;
                if purge {
                    bail!(
                        "--purge drops one instance's database, and grove would have to load \
                         every worktree's config to find the others'\n\
                         run `grove down --purge` in each worktree whose data you want gone"
                    );
                }
                return sweep(&instance.resolved.worktree, idle.as_deref(), dry_run);
            }

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
                print_services(&instance);
                warn_if_stale(&instance);
            }
            Ok(())
        }
        Some(Command::Ls { json }) => {
            let registry = grove::instance::registry()?;
            // Listing is a read. Reaping here would discard a deleted worktree's pids
            // without stopping its services, leaving processes grove can never reach.
            let mut entries = registry.list()?;
            let now = grove::registry::now();

            // Most neglected first. Eighteen rows with the stale ones scattered through
            // them is how a pile-up goes unnoticed; the reclaim candidates belong on top.
            // Instances still in use sort last, since they are nobody's candidate.
            entries.sort_by(|a, b| {
                let key = |e: &grove::registry::Entry| {
                    (
                        std::cmp::Reverse(e.is_running()),
                        std::cmp::Reverse(e.idle_seconds(now).unwrap_or(0)),
                    )
                };
                key(a).cmp(&key(b)).then_with(|| a.slug.cmp(&b.slug))
            });

            let machine = survey(None)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ls_json(&entries, now, &machine))?
                );
                return Ok(());
            }

            if entries.is_empty() {
                println!("no instances");
            }
            let orphans = entries.iter().filter(|e| !e.worktree.exists()).count();
            for entry in &entries {
                // Ask the processes, not the record. A recorded pid says only that a
                // service was started once; reading that as "up" turns a stopped
                // instance into an apparent port conflict.
                let live = entry
                    .services
                    .values()
                    .filter(|h| grove::supervise::is_alive(h))
                    .count();
                let state = if !entry.worktree.exists() {
                    format!("orphaned ({live})")
                } else if live > 0 {
                    format!("running ({live})")
                } else {
                    "stopped".to_string()
                };
                // Only for what is running: how long a stopped instance has been stopped
                // is not a question anyone is asking here.
                let idle = match entry.idle_seconds(now).filter(|_| live > 0) {
                    Some(age) => format!("idle {}", human_age(age)),
                    None => String::new(),
                };
                let ports: Vec<String> = entry
                    .ports
                    .iter()
                    .map(|(n, p)| format!("{n}={p}"))
                    .collect();
                let exposure = if entry.exposure.is_exposed() {
                    format!(" exposed {}", entry.exposure.public_host())
                } else {
                    String::new()
                };
                println!(
                    "{:<28} {:<13} {:<10} {}{}",
                    entry.slug,
                    state,
                    idle,
                    ports.join(" "),
                    exposure
                );
            }
            if orphans > 0 {
                println!(
                    "\n{orphans} orphaned — their worktree is gone. `grove prune` stops them."
                );
            }

            // The facts unconditionally — this is where someone deciding what to stop is
            // already looking. The call to action only when there is a pile-up to act on:
            // the idle column has already said which rows are stale, and proposing a
            // sweep on a quiet two-instance machine is how a prescription becomes noise.
            if !entries.is_empty() {
                println!("\n{}", machine.headline());
                if machine.crowded() {
                    for line in machine.prescription().unwrap_or_default() {
                        println!("{line}");
                    }
                }
            }
            Ok(())
        }
        Some(Command::Run { argv }) => {
            let instance = Instance::open(&cwd)?;
            instance.touch()?;
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(&cwd)
                .envs(instance.environment()?)
                .status()
                .with_context(|| format!("running `{}`", argv.join(" ")))?;

            // Sampled after the child, so the average includes the contention the run
            // actually experienced — and printed only on failure, because a note that
            // appears after every green run is wallpaper by the second day.
            if !status.success() {
                let machine = survey(Some(&instance.resolved.worktree))?;
                if machine.crowded() {
                    eprintln!("note: {}", machine.headline());
                    eprintln!(
                        "  a timeout here may be the machine rather than your branch — `grove ls`"
                    );
                }
            }
            std::process::exit(status.code().unwrap_or(1));
        }
        Some(Command::Logs {
            service,
            follow,
            lines,
            since_restart,
        }) => {
            let instance = Instance::open(&cwd)?;
            instance.touch()?;
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
                Ok(body) => {
                    // The build log is replayed first otherwise, so the head of `logs`
                    // shows dependency resolution rather than the service booting.
                    let body = if since_restart {
                        match body.rfind(grove::supervise::START_MARKER) {
                            Some(at) => body[at..].to_string(),
                            None => body,
                        }
                    } else {
                        body
                    };
                    let body = match lines {
                        Some(n) => {
                            let kept: Vec<&str> = body.lines().collect();
                            let from = kept.len().saturating_sub(n);
                            format!("{}\n", kept[from..].join("\n"))
                        }
                        None => body,
                    };
                    print!("{body}");
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    bail!("{name} has no log yet at {}", path.display())
                }
                Err(e) => return Err(e).context("reading the log"),
            }
            Ok(())
        }
        Some(Command::Restart { service }) => {
            let mut instance = Instance::open(&cwd)?;
            instance.refuse_in_main()?;
            instance.touch()?;
            for name in instance.stop_services(service.as_deref())? {
                println!("stopped {name}");
            }
            // `up` only starts what is not already running, so stopping first is what
            // makes this a restart rather than a no-op.
            instance.up(false)?;
            print_summary(&instance);
            Ok(())
        }
        Some(Command::Prune { purge, dry_run }) => {
            let registry = grove::instance::registry()?;

            // A dry run must not reap: `reap` removes the entries as it returns them, so
            // rehearsing with it would be the destructive act it exists to avoid.
            let orphans: Vec<Entry> = if dry_run {
                registry
                    .list()?
                    .into_iter()
                    .filter(|e| !e.worktree.exists())
                    .collect()
            } else {
                // Stop the services before dropping the entry: once it is gone the pids go
                // with it, and nothing can reach those processes again.
                registry.reap()?
            };

            if orphans.is_empty() {
                println!("nothing to reclaim");
                return Ok(());
            }

            for orphan in &orphans {
                if !dry_run {
                    for handle in orphan.services.values() {
                        grove::supervise::stop(handle)?;
                    }
                }
                let ports: Vec<String> = orphan
                    .ports
                    .iter()
                    .map(|(n, p)| format!("{n}={p}"))
                    .collect();
                let verb = if dry_run {
                    "would reclaim"
                } else {
                    "reclaimed"
                };
                println!("{verb} {:<28} {}", orphan.slug, ports.join(" "));

                // Always name the database, whatever happens to it. It outlives the
                // worktree either way, and a number nobody named is a number nobody audits.
                let Some(database) = &orphan.db_name else {
                    continue;
                };
                match (&orphan.db_resource, purge, dry_run) {
                    (_, false, _) => {
                        println!("  database {database} left in place (--purge drops it)")
                    }
                    (Some(_), true, true) => println!("  database {database} would be dropped"),
                    (Some(at), true, false) => {
                        match grove::resource::drop_database_at(&at.name, at.port, database) {
                            Ok(()) => println!("  database {database} dropped"),
                            Err(e) => println!("  database {database} left in place: {e:#}"),
                        }
                    }
                    // Written before grove recorded where the datastore was, and the
                    // worktree that could have said is already gone. Hand over the command
                    // rather than imply this was handled.
                    (None, true, _) => {
                        println!(
                            "  database {database} left in place — this instance predates \
                             grove recording its datastore, so grove cannot reach it"
                        );
                        println!(
                            "    {}",
                            grove::resource::drop_database_hint(27017, database)
                        );
                    }
                }
            }
            if dry_run {
                println!(
                    "\n{} orphaned. Nothing reclaimed (--dry-run).",
                    orphans.len()
                );
            }
            Ok(())
        }
        Some(Command::Seed { force }) => {
            let instance = Instance::open(&cwd)?;
            instance.refuse_in_main()?;
            instance.touch()?;
            for outcome in instance.seed(force)? {
                println!("{outcome}");
            }
            Ok(())
        }
        Some(Command::Doctor) => {
            let instance = Instance::open(&cwd)?;
            let verdicts = grove::doctor::check(&instance);
            if grove::doctor::report(&verdicts)? {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::Skill {
            action: SkillAction::Install,
        }) => {
            let installed = grove::skill::install()?;
            println!("installed the grove skill to {}", installed.path.display());
            if let Some(removed) = installed.removed {
                println!("removed the old treeish skill at {}", removed.display());
            }
            Ok(())
        }
    }
}

/// A service older than the newest edit is serving code you have already changed. Without
/// this, the next surprising response reads as a bug in the code you are looking at.
fn warn_if_stale(instance: &Instance) {
    let stale = instance.stale_services();
    if stale.is_empty() {
        return;
    }
    println!();
    for name in &stale {
        println!("{name} started before your newest edit and may be serving stale code");
    }
    println!("  grove restart {}", stale.join(" "));
}

/// Stop instances across the machine, keeping their port reservations — the difference
/// between this and `prune`, and the reason it is spelled as `down`: a forgotten box is
/// one you want back tomorrow on the ports whose URLs are already written down.
///
/// `idle` of None means every running instance but this one.
fn sweep(here: &Path, idle: Option<&str>, dry_run: bool) -> Result<()> {
    let registry = grove::instance::registry()?;
    let now = grove::registry::now();
    let window = idle.map(grove::instance::parse_duration).transpose()?;

    let running: Vec<Entry> = registry
        .list()?
        .into_iter()
        .filter(Entry::is_running)
        .collect();

    // Never the instance the caller is standing in. Weak protection — it does nothing for
    // a sibling agent — but it removes the one outcome nobody would expect, and someone
    // three hours into debugging here has issued no grove command to prove it.
    let mut doomed: Vec<(&Entry, u64)> = running
        .iter()
        .filter(|e| e.worktree != here)
        .filter_map(|e| match (window, e.idle_seconds(now)) {
            (None, age) => Some((e, age.unwrap_or(0))),
            (Some(w), Some(age)) if age >= w.as_secs() => Some((e, age)),
            // No evidence either way is not evidence of neglect.
            (Some(_), _) => None,
        })
        .collect();
    doomed.sort_by_key(|(_, age)| std::cmp::Reverse(*age));

    if doomed.is_empty() {
        println!("nothing to stop — {} running", running.len());
        return Ok(());
    }

    // Named, always. This command's blast radius reaches other people's work, and a
    // count cannot be checked against what you know about who is doing what.
    let width = doomed.iter().map(|(e, _)| e.slug.len()).max().unwrap_or(0);
    for (entry, age) in &doomed {
        let verb = if dry_run { "would stop" } else { "stopped" };
        println!(
            "{verb} {:<width$}  (idle {})",
            entry.slug,
            human_age(*age),
            width = width
        );
    }

    if dry_run {
        println!(
            "\n{} of {} running. Nothing stopped (--dry-run).",
            doomed.len(),
            running.len()
        );
        return Ok(());
    }

    for (entry, _) in &doomed {
        let mut entry = (*entry).clone();
        for handle in entry.services.values() {
            grove::supervise::stop(handle)?;
        }
        entry.services.clear();
        registry.record(&entry)?;
    }
    println!(
        "\n{} stopped, ports kept. {} still running.",
        doomed.len(),
        running.len() - doomed.len()
    );
    Ok(())
}

/// The machine-wide counterpart to `status --json`: everything an agent needs to decide
/// whether the box is the problem, without parsing a table or shelling out to `uptime`.
fn ls_json(entries: &[Entry], now: u64, machine: &Machine) -> serde_json::Value {
    let instances: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "slug": e.slug,
                "worktree": e.worktree,
                "running": e.is_running(),
                "orphaned": !e.worktree.exists(),
                "idle_seconds": e.idle_seconds(now),
                "exposed": e.exposure.is_exposed(),
                "public_host": e.exposure.public_host(),
                "ports": e.ports,
                "database": e.db_name,
            })
        })
        .collect();

    serde_json::json!({
        "load": machine.load.as_ref().map(|l| l.one),
        "cores": machine.load.as_ref().map(|l| l.cores),
        "running": machine.running,
        "crowded": machine.crowded(),
        "instances": instances,
    })
}

/// What each service is doing, for `status` only.
///
/// Deliberately not folded into `print_summary`, which `up` and `restart` also call: `up`
/// has just waited on the readiness probe, so probing again there would spend the timeout
/// to re-learn what it already blocked on.
fn print_services(instance: &Instance) {
    let services = instance.status();
    if services.is_empty() {
        return;
    }
    let width = services
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0)
        .max(8);

    // Headed and indented, because port names and service names are separate namespaces
    // that often collide — a bare `web` row under the `web` URL above reads as the same
    // thing said twice.
    println!("\nservices");
    for service in &services {
        let state = match (service.running, service.pid) {
            (true, Some(pid)) => format!("running  pid {pid}"),
            (true, None) => "running".to_string(),
            (false, _) => "stopped".to_string(),
        };
        let name = format!("  {:<width$}", service.name, width = width);

        // The loud case, because it is the whole reason this command probes: the process
        // survives, so every cheaper signal says fine, and the endpoint is dead.
        match service.ready {
            grove::instance::Readiness::Silent if service.running => {
                println!(
                    "{name}  {state}  NOT ANSWERING {}",
                    service.url.as_deref().unwrap_or("")
                );
                println!(
                    "  {:<width$}  grove logs {} --since-restart",
                    "",
                    service.name,
                    width = width
                );
            }
            grove::instance::Readiness::Answering => println!("{name}  {state}  answering"),
            _ => println!("{name}  {state}"),
        }
    }
}

fn status_json(instance: &Instance) -> serde_json::Value {
    let services: serde_json::Map<String, serde_json::Value> = instance
        .status()
        .into_iter()
        .map(|s| {
            (
                s.name,
                serde_json::json!({
                    "running": s.running,
                    "pid": s.pid,
                    "ready": s.ready.as_str(),
                    "url": s.url,
                }),
            )
        })
        .collect();
    serde_json::json!({
        "slug": instance.resolved.slug,
        "worktree": instance.resolved.worktree,
        "exposed": instance.exposure().is_exposed(),
        "public_host": instance.exposure().public_host(),
        "ports": instance.entry.ports,
        "database": instance.db_name(),
        "services": services,
    })
}

/// The only thing an agent should need to read after `up`.
fn print_summary(instance: &Instance) {
    println!("instance  {}", instance.resolved.slug);
    for (name, port) in &instance.entry.ports {
        println!(
            "{name:<10}http://{}:{port}",
            instance.exposure().public_host()
        );
    }
    if instance.exposure().is_exposed() {
        println!("exposure  network ({})", instance.exposure().public_host());
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
    println!("  grove run -- <your test command>");
    println!("  grove logs {first} -f");
}
