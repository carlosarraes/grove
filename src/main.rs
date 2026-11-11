mod up_log;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use grove::footprint::human_size;
use grove::registry::{Entry, human_age};
use grove::{instance::Instance, llm, load};
use std::path::{Path, PathBuf};

/// The window the printed prescription proposes; `health` reads the same one.
const IDLE_WINDOW: &str = grove::health::IDLE_WINDOW;

fn herdr_owner() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.is_empty())
        .map(|pane| format!("herdr pane {pane}"))
}

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
#[command(
    name = "grove",
    version,
    about,
    disable_help_subcommand = true,
    after_help = "Worktree commands act on the worktree you run them in. `ls`, `health`, `prune` and \
                  `clean` are machine-wide. `ls`, `status`, `doctor`, `down`, `gate`, `prune` and \
                  `clean` accept instance names as `grove ls` shows them, or worktree paths: \
                  `grove status feature_branch`. `--worktree` is an alias for the positional \
                  target on `ls`, `status` and `gate`; it is repeatable on `prune` and `clean`."
)]
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
        /// Internal worker for the per-run log supervisor.
        #[arg(long, hide = true)]
        up_log_child: bool,
        /// Wait for a machine-wide startup slot and load admission
        #[arg(long)]
        heavy: bool,
        /// Hold until load is under the install gate: `[admission] link_only` (default
        /// 60/60) when the store already has every dependency tree, `full_install`
        /// (default 25/35) when one must be built
        #[arg(long)]
        gate: bool,
        /// Run dependency setup only (link from the store on a cache hit, install on a miss); start no shared containers, seeds or services
        #[arg(long, conflicts_with_all = ["fresh", "expose", "expose_host"])]
        setup_only: bool,
        /// Report whether setup would link or install; make no changes (requires --setup-only)
        #[arg(long, requires = "setup_only")]
        dry_run: bool,
        /// Restart services that are already running
        #[arg(long)]
        fresh: bool,
        /// Measure the dependency footprint after setup (walks the worktree; off by default)
        #[arg(long)]
        footprint: bool,
        /// Permit running in the main worktree, overwriting its real env files
        #[arg(long)]
        allow_main: bool,
        /// Expose opted-in services to the local network using the default-route IPv4
        #[arg(long)]
        expose: bool,
        /// Expose to the local network using this IPv4 address or hostname
        #[arg(long, value_name = "HOST")]
        expose_host: Option<String>,
        /// Reinstall dependencies from scratch and replace the store entry they came from
        #[arg(long)]
        no_cache: bool,
        /// Limit setup/start to named services (repeatable)
        #[arg(long = "service", value_name = "NAME", action = clap::ArgAction::Append)]
        services: Vec<String>,
    },
    /// Stop this instance's services, or the named one's
    Down {
        /// Instance name as `grove ls` shows it, or a worktree path [default: the worktree you are in]
        #[arg(conflicts_with_all = ["idle", "all_but_this"])]
        name: Option<String>,
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
        /// Instance name as `grove ls` shows it, or a worktree path [default: the worktree you are in]
        #[arg(conflicts_with = "worktree")]
        name: Option<String>,
        /// Instance name as `grove ls` shows it, or a worktree path
        #[arg(long, value_name = "NAME|PATH", conflicts_with = "name")]
        worktree: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Whether `up` here would link or build dependencies, and whether load admits it now;
    /// exits 0 when admitted, 3 when holding
    Gate {
        /// Instance name as `grove ls` shows it, or a worktree path [default: the worktree you are in]
        #[arg(conflicts_with = "worktree")]
        name: Option<String>,
        /// Instance name as `grove ls` shows it, or a worktree path
        #[arg(long, value_name = "NAME|PATH", conflicts_with = "name")]
        worktree: Option<String>,
        /// Judge as `up --no-cache` would: always a full install
        #[arg(long)]
        no_cache: bool,
        /// Limit setup/start to named services (repeatable)
        #[arg(long = "service", value_name = "NAME", action = clap::ArgAction::Append)]
        services: Vec<String>,
    },
    /// Every instance on this machine
    #[command(visible_alias = "list", alias = "instances")]
    Ls {
        /// Instance name as `grove ls` shows it, or a worktree path
        #[arg(conflicts_with = "worktree")]
        name: Option<String>,
        /// Instance name as `grove ls` shows it, or a worktree path
        #[arg(long, value_name = "NAME|PATH", conflicts_with = "name")]
        worktree: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run a command with this instance's environment overlaid
    Run {
        /// Wait for load below the admission threshold before starting the command
        #[arg(long)]
        heavy: bool,
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
    /// Remove idle grove-linked git worktrees (dry-run by default); prune forgets orphaned instances
    Clean {
        /// Instance names as `grove ls` shows them, or recorded worktree paths
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Select by recorded worktree path (repeatable)
        #[arg(long = "worktree", value_name = "PATH", action = clap::ArgAction::Append)]
        worktrees: Vec<String>,
        /// Delete eligible worktrees; without this flag clean is a dry run
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
        /// Explicit dry run (the default); change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop and forget all orphaned instances, or only the selected orphans
    Prune {
        /// Instance names as `grove ls` shows them, or recorded worktree paths
        #[arg(value_name = "NAME")]
        names: Vec<String>,
        /// Select by recorded worktree path (repeatable); selected prune skips the shared store sweep
        #[arg(long = "worktree", value_name = "PATH", action = clap::ArgAction::Append)]
        worktrees: Vec<String>,
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
    Doctor {
        /// Instance name as `grove ls` shows it, or a worktree path [default: the worktree you are in]
        name: Option<String>,
    },
    /// Everything on this machine that is costing someone, and what ends it
    Health {
        #[arg(long)]
        json: bool,
    },
    /// Report stale Mongo testcontainers without changing them
    TestContainers {
        /// Recheck and remove eligible containers by exact ID
        #[arg(long)]
        remove_stale: bool,
    },
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

    // Rust ignores SIGPIPE, so a reader that closes early (`grove ls | head`) turns the
    // next print into a panic. Every command but `up` is a plain reporter: let a closed
    // stdout end it quietly, as it would `cat`. `up` keeps ignoring it on purpose — its
    // wrapper must outlive a closed terminal to finish the run and its log.
    if !matches!(cli.command, Some(Command::Up { .. })) {
        // SAFETY: called before any thread starts; only resets a signal disposition.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    if cli.llm {
        print!("{}", llm::reference());
        return Ok(());
    }

    let cwd = std::env::current_dir().context("reading the working directory")?;

    if matches!(
        cli.command,
        Some(Command::Up {
            up_log_child: false,
            ..
        })
    ) {
        std::process::exit(up_log::run(&cwd)?);
    }

    match cli.command {
        None => {
            <Cli as clap::CommandFactory>::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Command::Gate {
            name,
            worktree,
            no_cache,
            services,
        }) => {
            let instance = Instance::open(&target(name.as_deref().or(worktree.as_deref()), &cwd)?)?;
            instance.validate_services(&services)?;
            let install = instance.install_plan(no_cache, &services);
            let verdict = grove::load::judge(
                &install,
                &instance.config.admission.clone().unwrap_or_default(),
                grove::load::averages(),
            );
            println!("{}", verdict.line);
            if !verdict.admitted {
                std::process::exit(3);
            }
            Ok(())
        }
        Some(Command::Up {
            up_log_child: _,
            heavy,
            gate,
            setup_only,
            dry_run,
            fresh,
            footprint,
            allow_main,
            expose,
            expose_host,
            no_cache,
            services,
        }) => {
            let began = std::time::Instant::now();
            let _up_lock = grove::instance::lock_up(&cwd)?;
            let exposure = if setup_only {
                grove::exposure::Exposure::local()
            } else {
                match expose_host {
                    Some(host) => grove::exposure::Exposure::explicit(&host)?,
                    None if expose => grove::exposure::Exposure::detect()?,
                    None => grove::exposure::Exposure::local(),
                }
            };
            let mut instance = Instance::open(&cwd)?;
            instance.validate_services(&services)?;
            if !allow_main {
                instance.refuse_in_main()?;
            }
            if !dry_run {
                grove::instance::registry()?
                    .set_started_by(&instance.resolved.worktree, herdr_owner())?;
            }
            // Before the waits, as `run --heavy` does: a held `up` has started, and a
            // stale clock would let a sibling's sweep stop its services mid-wait.
            if !dry_run {
                instance.touch()?;
            }
            let _startup_slot = if heavy {
                Some(grove::load::wait_for_startup(
                    &instance.config.admission.clone().unwrap_or_default(),
                )?)
            } else {
                None
            };
            if gate {
                let install = instance.install_plan(no_cache, &services);
                grove::load::wait_for_install_gate(
                    &install,
                    &instance.config.admission.clone().unwrap_or_default(),
                )?;
            }
            if !dry_run {
                instance.touch()?;
            }
            if !setup_only && !dry_run {
                enforce_idle(&instance)?;
            }

            if !setup_only && exposure.is_exposed() {
                eprintln!(
                    "warning: exposing {} on all interfaces as {}; development services and authentication bypasses may be reachable from other machines",
                    instance.resolved.slug,
                    exposure.public_host()
                );
            }

            if setup_only {
                if dry_run {
                    let plans = instance.setup_install_plan(no_cache, &services);
                    for (name, plan) in &plans {
                        eprintln!("{}", setup_plan_line(name, plan));
                    }
                    eprintln!(
                        "setup-only dry-run: {} services, 0 installed or linked",
                        plans.len()
                    );
                } else {
                    instance.render()?;
                    let outcomes = instance.setup_only(no_cache, footprint, &services)?;
                    for (service, outcome) in outcomes {
                        eprintln!("{}", setup_outcome_line(&service, outcome));
                    }
                    eprintln!(
                        "setup-only: {} services set up, 0 started",
                        instance
                            .config
                            .services
                            .iter()
                            .filter(|s| {
                                s.setup.is_some() && Instance::service_selected(&s.name, &services)
                            })
                            .count()
                    );
                }
            } else {
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
                for outcome in instance.up(fresh, no_cache, footprint, &services)? {
                    eprintln!("{outcome}");
                }
                print_summary(&instance, None, &[]);
                // Last, and only here: an agent waiting on `up` needs one line that
                // means "done and serving", distinct from every progress line before it.
                println!(
                    "\ngrove: ready {} ({}s)",
                    instance.resolved.slug,
                    began.elapsed().as_secs()
                );
            }
            Ok(())
        }
        Some(Command::Down {
            name,
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

            let worktree = target(name.as_deref(), &cwd)?;
            // An up still working here would start services after this reports them
            // stopped. Stop it first, then hold its lock so none starts mid-teardown.
            let stopped_up = grove::instance::stop_running_up(&worktree)?;
            let _up_lock = grove::instance::lock_up(&worktree)?;
            if let Some(pid) = stopped_up {
                println!("stopped the running grove up (pid {pid}) first");
            }
            let mut instance = Instance::open(&worktree)?;
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
            if let Some(line) = kept_line(&instance) {
                println!("{line}");
            }
            Ok(())
        }
        Some(Command::Status {
            name,
            worktree,
            json,
        }) => {
            let worktree = match locate(name.as_deref().or(worktree.as_deref()), &cwd)? {
                Target::Worktree(path) => path,
                Target::Orphan(entry, prune) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&orphan_status_json(&entry, &prune))?
                        );
                    } else {
                        print_orphan_status(&entry, &prune);
                    }
                    return Ok(());
                }
            };
            let instance = Instance::open_for_status(&worktree)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status_json(&instance))?);
            } else {
                print_summary(&instance, elsewhere(&instance, &cwd), &attention(&instance));
                print_services(&instance);
                warn_if_stale(&instance);
            }
            Ok(())
        }
        Some(Command::Ls {
            name,
            worktree,
            json,
        }) => {
            let registry = grove::instance::registry()?;
            // Listing is a read. Reaping here would discard a deleted worktree's pids
            // without stopping its services, leaving processes grove can never reach.
            let mut entries = registry.list()?;
            if let Some(ask) = name.as_deref().or(worktree.as_deref()) {
                let requested = match locate(Some(ask), &cwd)? {
                    Target::Worktree(path) => path,
                    Target::Orphan(entry, _) => entry.worktree.clone(),
                };
                entries.retain(|entry| entry.worktree == requested || entry.slug == ask);
                if entries.is_empty() {
                    bail!("no listed instance for `{ask}`; `grove ls` lists instances");
                }
            }
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

            let port_labels: Vec<String> = entries
                .iter()
                .map(|entry| {
                    let ports: Vec<String> = entry
                        .ports
                        .iter()
                        .map(|(name, port)| format!("{name}={port}"))
                        .collect();
                    let exposure = if entry.exposure.is_exposed() {
                        format!(" exposed {}", entry.exposure.public_host())
                    } else {
                        String::new()
                    };
                    format!("{}{exposure}", ports.join(" "))
                })
                .collect();
            let port_width = port_labels
                .iter()
                .map(String::len)
                .max()
                .unwrap_or(0)
                .max(5);
            if entries.is_empty() {
                println!("no instances");
            } else {
                println!(
                    "{:<28} {:<13} {:<10} {:>5}  {:<port_width$}  OWNER",
                    "INSTANCE", "STATE", "IDLE", "SIZE", "PORTS"
                );
            }
            let orphans = entries.iter().filter(|e| e.is_orphaned()).count();
            let mut failed = 0;
            for (entry, ports) in entries.iter().zip(&port_labels) {
                // Ask the processes, not the record. A recorded pid says only that a
                // service was started once; reading that as "up" turns a stopped
                // instance into an apparent port conflict.
                let live = entry
                    .services
                    .values()
                    .filter(|h| grove::supervise::is_alive(h))
                    .count();
                let state = if entry.is_orphaned() {
                    format!("orphaned ({live})")
                } else if live > 0 {
                    format!("running ({live})")
                } else if let Some(last) = last_up(entry)
                    .filter(|last| last.failed() || last.interrupted() || last.running())
                {
                    if last.running() {
                        "starting".to_string()
                    } else if last.interrupted() {
                        "interrupted".to_string()
                    } else {
                        failed += 1;
                        "up failed".to_string()
                    }
                } else {
                    "stopped".to_string()
                };
                // Only for what is running: how long a stopped instance has been stopped
                // is not a question anyone is asking here.
                let idle = match entry.idle_seconds(now).filter(|_| live > 0) {
                    Some(age) => format!("idle {}", human_age(age)),
                    None => String::new(),
                };
                let size = disk_of(entry).map(human_size).unwrap_or_default();
                println!(
                    "{:<28} {:<13} {:<10} {:>5}  {:<port_width$}  {}",
                    entry.slug,
                    state,
                    idle,
                    size,
                    ports,
                    entry.started_by.as_deref().unwrap_or("-")
                );
            }
            if orphans > 0 {
                println!(
                    "\n{orphans} orphaned — their worktree is gone or no longer a git worktree. `grove prune` stops them."
                );
            }
            if failed > 0 {
                println!("\n{failed} whose last up failed. `grove status NAME` says why.");
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
                // Under the load line, because disk is the other resource a pile-up
                // exhausts, and it does so on a machine whose load looks fine.
                if let Some(line) = disk_footer(&entries) {
                    println!("{line}");
                }
            }
            Ok(())
        }
        Some(Command::Run { argv, heavy }) => {
            let instance = Instance::open(&cwd)?;
            instance.touch()?;
            enforce_idle(&instance)?;
            if heavy {
                load::wait_for_admission(&instance.config.admission.clone().unwrap_or_default())?;
                instance.touch()?;
            }
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
            let _up_lock = grove::instance::lock_up(&cwd)?;
            let mut instance = Instance::open(&cwd)?;
            instance.refuse_in_main()?;
            grove::instance::registry()?
                .set_started_by(&instance.resolved.worktree, herdr_owner())?;
            instance.touch()?;
            for name in instance.stop_services(service.as_deref())? {
                println!("stopped {name}");
            }
            // `up` only starts what is not already running, so stopping first is what
            // makes this a restart rather than a no-op.
            instance.up(false, false, false, &[])?;
            print_summary(&instance, None, &[]);
            Ok(())
        }
        Some(Command::Clean {
            names,
            worktrees,
            yes,
            dry_run: _,
        }) => {
            let selectors: Vec<String> = names.into_iter().chain(worktrees).collect();
            grove::clean::run(&cwd, &selectors, yes)
        }
        Some(Command::Prune {
            names,
            worktrees,
            purge,
            dry_run,
        }) => {
            let registry = grove::instance::registry()?;
            let selectors: Vec<String> = names.into_iter().chain(worktrees).collect();
            let selected = !selectors.is_empty();

            // A dry run must not reap: `reap` removes the entries as it returns them, so
            // rehearsing with it would be the destructive act it exists to avoid.
            let orphans: Vec<Entry> = if selected {
                let entries = registry.list()?;
                let mut chosen = std::collections::BTreeMap::new();
                for selector in &selectors {
                    let selected_path = Path::new(selector);
                    let selected_path = if selected_path.is_absolute() {
                        selected_path.to_path_buf()
                    } else {
                        cwd.join(selected_path)
                    };
                    let matches: Vec<&Entry> = entries
                        .iter()
                        .filter(|entry| entry.slug == *selector || entry.worktree == selected_path)
                        .collect();
                    let entry = match matches.as_slice() {
                        [entry] => *entry,
                        [] => bail!(
                            "no instance named or recorded at `{selector}`; `grove ls` lists instances"
                        ),
                        several => bail!(
                            "{} instances match `{selector}`; select a worktree path instead",
                            several.len()
                        ),
                    };
                    if !entry.is_orphaned() {
                        bail!(
                            "`{selector}` still has its worktree at {}; `grove down` stops it",
                            entry.worktree.display()
                        );
                    }
                    chosen.insert(entry.worktree.clone(), entry.clone());
                }
                chosen.into_values().collect()
            } else if dry_run {
                registry
                    .list()?
                    .into_iter()
                    .filter(|e| e.is_orphaned())
                    .collect()
            } else {
                // Stop the services before dropping the entry: once it is gone the pids go
                // with it, and nothing can reach those processes again.
                registry.reap()?
            };

            // After the reap, so a reaped orphan's key no longer counts. Referenced means a
            // worktree that still exists recorded it: an entry only a deleted worktree
            // knew about would never be linked from again.
            let referenced: std::collections::HashSet<String> = registry
                .list()?
                .iter()
                .filter(|e| !e.is_orphaned())
                .flat_map(|e| e.cache_keys.values().cloned())
                .collect();
            let store = grove::store::dir()?;
            let planned_swept = if selected {
                Vec::new()
            } else {
                grove::store::unreferenced(&store, &referenced)?
            };
            let stale_temp_count = planned_swept
                .iter()
                .filter(|name| name.rsplit_once(".tmp-").is_some())
                .count();
            let stale_temp_bytes = grove::store::stale_temp_bytes(&store, &planned_swept)?;
            let swept = if selected {
                Vec::new()
            } else if dry_run {
                planned_swept.clone()
            } else {
                grove::store::gc(&store, &referenced)?
            };

            if orphans.is_empty() && swept.is_empty() {
                println!("nothing to reclaim");
                return Ok(());
            }

            let mut left = 0;
            for orphan in &orphans {
                if !dry_run {
                    for handle in orphan.services.values() {
                        grove::supervise::stop(handle)?;
                    }
                    if selected {
                        registry.release(&orphan.worktree)?;
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
                // The database rides on the row, whatever happens to it. It outlives the
                // worktree either way, and the count printed at the end is not an audit:
                // a number nobody named is a number nobody audits.
                let database = orphan
                    .db_name
                    .as_deref()
                    .map(|db| format!("  database {db}"))
                    .unwrap_or_default();
                println!("{verb} {:<28} {}{database}", orphan.slug, ports.join(" "));

                let Some(database) = &orphan.db_name else {
                    continue;
                };
                match (&orphan.db_resource, purge, dry_run) {
                    (_, false, _) => left += 1,
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
            if selected {
                println!("store sweep skipped; store entries are shared");
            }
            // Once, not once per orphan: eighteen "left in place" lines said eighteen
            // times what one line with the number in it says better.
            if left > 0 || dry_run {
                println!();
            }
            match left {
                0 => {}
                1 => println!("1 database left in place (--purge drops it)"),
                n => println!("{n} databases left in place (--purge drops them)"),
            }
            for name in swept
                .iter()
                .filter(|name| name.rsplit_once(".tmp-").is_some())
            {
                println!(
                    "{} {name}",
                    if dry_run { "would remove" } else { "removed" }
                );
            }
            let store_entries = swept.len().saturating_sub(stale_temp_count);
            match store_entries {
                0 => {}
                n => println!(
                    "{} {n} store {} no worktree references",
                    if dry_run { "would remove" } else { "removed" },
                    if n == 1 { "entry" } else { "entries" }
                ),
            }
            if stale_temp_count > 0 {
                println!(
                    "{} {stale_temp_count} stale store temp {} ({})",
                    if dry_run { "would remove" } else { "removed" },
                    if stale_temp_count == 1 {
                        "path"
                    } else {
                        "paths"
                    },
                    human_size(stale_temp_bytes)
                );
            }
            if dry_run {
                println!("{} orphaned. Nothing reclaimed (--dry-run).", orphans.len());
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
        Some(Command::TestContainers { remove_stale }) => grove::test_containers::run(remove_stale),
        Some(Command::Health { json }) => {
            let verdicts = grove::health::check(&cwd)?;
            let healthy = if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&grove::health::json(&verdicts))?
                );
                !verdicts
                    .iter()
                    .any(|v| matches!(v, grove::doctor::Verdict::Fail { .. }))
            } else {
                grove::doctor::report(&verdicts)?
            };
            if healthy {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::Doctor { name }) => {
            let instance = Instance::open(&target(name.as_deref(), &cwd)?)?;
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

/// What a command was pointed at: a worktree it can resolve, or an orphaned instance that
/// only the registry still knows about.
enum Target {
    Worktree(PathBuf),
    Orphan(Box<Entry>, String),
}

/// The worktree a command acts on: the one it runs in, or the one named by an instance
/// name as `grove ls` prints it, or by a path. A name is looked up before a path, so
/// `grove status backend` inside a repo means the instance, not the `backend/` folder.
fn target(name: Option<&str>, cwd: &Path) -> Result<PathBuf> {
    match locate(name, cwd)? {
        Target::Worktree(path) => Ok(path),
        Target::Orphan(entry, prune) => bail!(
            "instance `{}` is orphaned: its worktree {} {}; `{prune}` stops what it left running and forgets it",
            entry.slug,
            entry.worktree.display(),
            entry
                .orphaned()
                .map_or_else(String::new, |orphan| orphan.to_string()),
        ),
    }
}

/// Like [`target`], but an orphan is an answer rather than an error, for the commands that
/// can report on one.
fn locate(name: Option<&str>, cwd: &Path) -> Result<Target> {
    let entries = grove::instance::registry()?.list()?;
    let Some(name) = name else {
        return Ok(at_path(cwd, &entries));
    };
    let matches: Vec<&Entry> = entries.iter().filter(|e| e.slug == name).collect();
    match matches.as_slice() {
        [entry] if entry.is_orphaned() => {
            return Ok(Target::Orphan(
                Box::new((*entry).clone()),
                prune_command(entry, &entries),
            ));
        }
        [entry] => return Ok(Target::Worktree(entry.worktree.clone())),
        [] => {}
        several => bail!(
            "{} instances are named `{name}`; pass the worktree path instead:\n{}",
            several.len(),
            several
                .iter()
                .map(|e| format!("  {}", e.worktree.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
    let path = cwd.join(name);
    if path.is_dir() {
        return Ok(at_path(&path, &entries));
    }
    if name.contains('/') || name.starts_with('.') || name.starts_with('~') {
        bail!(
            "no worktree at {name}; pass an existing worktree path, or an instance name from `grove ls`"
        );
    }
    bail!(
        "no instance named `{name}` on this machine, and no directory by that name; `grove ls` lists instances"
    )
}

/// A directory inside an orphan's husk resolves to no git worktree, and git's own error
/// would send the reader looking for a typo. The innermost recorded instance holding the
/// path decides, so a live worktree is never mistaken for an orphan around it.
fn at_path(path: &Path, entries: &[Entry]) -> Target {
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let holder = entries
        .iter()
        .filter(|e| path.starts_with(&e.worktree) || real.starts_with(&e.worktree))
        .max_by_key(|e| e.worktree.components().count());
    // Only the directories between `path` and the orphan's root: above it, a dotfiles
    // repo in $HOME would count as a worktree and hide every orphan.
    let nested_worktree = |entry: &Entry| {
        let inside = if real.starts_with(&entry.worktree) {
            real.as_path()
        } else {
            path
        };
        inside
            .ancestors()
            .take_while(|dir| *dir != entry.worktree)
            .any(grove::resolve::is_worktree)
    };
    match holder {
        Some(entry) if entry.is_orphaned() && !nested_worktree(entry) => {
            Target::Orphan(Box::new(entry.clone()), prune_command(entry, entries))
        }
        _ => Target::Worktree(path.to_path_buf()),
    }
}

/// The exact command that reclaims `entry`: by name when the name is unambiguous, which is
/// how `grove ls` shows it, and by path otherwise.
fn prune_command(entry: &Entry, entries: &[Entry]) -> String {
    if entries.iter().filter(|e| e.slug == entry.slug).count() == 1 {
        format!("grove prune {}", entry.slug)
    } else {
        format!("grove prune --worktree {}", entry.worktree.display())
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

/// Running instances past the window, oldest first. `repo` narrows the field to one
/// repo's instances — the ones whose config declared the window — by the state directory
/// they share, since the registry does not record which checkout an instance belongs to.
///
/// Never the instance the caller is standing in. Weak protection — it does nothing for
/// a sibling agent — but it removes the one outcome nobody would expect, and someone
/// three hours into debugging here has issued no grove command to prove it.
fn idle_candidates<'a>(
    running: &'a [Entry],
    here: &Path,
    window: Option<std::time::Duration>,
    now: u64,
    repo: Option<&Path>,
) -> Vec<(&'a Entry, u64)> {
    let mut doomed: Vec<(&Entry, u64)> = running
        .iter()
        .filter(|e| e.worktree != here)
        .filter(|e| {
            repo.is_none_or(|repo| e.instance_dir.as_deref().and_then(Path::parent) == Some(repo))
        })
        .filter_map(|e| match (window, e.idle_seconds(now)) {
            (None, age) => Some((e, age.unwrap_or(0))),
            (Some(w), Some(age)) if age >= w.as_secs() => Some((e, age)),
            // No evidence either way is not evidence of neglect.
            (Some(_), _) => None,
        })
        .collect();
    doomed.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
    doomed
}

/// Stop this repo's sibling instances that have had no grove command within its
/// `[idle] stop_after` (4h unless the repo says otherwise, "off" to opt out), ports kept.
/// The pile-up forms one agent at a time and `up` and `run` are the moments the agent
/// adding to it is present. On stderr so a `run` child's stdout stays the child's.
fn enforce_idle(instance: &Instance) -> Result<()> {
    let Some(window) = instance.config.idle_window()? else {
        return Ok(());
    };
    let registry = grove::instance::registry()?;
    let now = grove::registry::now();
    let running: Vec<Entry> = registry
        .list()?
        .into_iter()
        .filter(Entry::is_running)
        .collect();
    // Without a state dir there is no telling which repo this is, and an unscoped sweep
    // would reach other repos' instances.
    let Some(repo) = instance
        .entry
        .instance_dir
        .as_deref()
        .and_then(Path::parent)
    else {
        return Ok(());
    };
    let doomed = idle_candidates(
        &running,
        &instance.resolved.worktree,
        Some(window),
        now,
        Some(repo),
    );
    for (entry, age) in doomed {
        if !registry.stop_if_idle(entry, window)? {
            continue;
        }
        eprintln!(
            "grove: stopped idle {} (no grove command for {})",
            entry.slug,
            human_age(age)
        );
    }
    Ok(())
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
    let doomed = idle_candidates(&running, here, window, now, None);

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
    // Whoever sweeps is deciding whether the machine has room, and "ports kept" alone
    // reads as if the sweep made some.
    let held: u64 = doomed.iter().filter_map(|(e, _)| disk_of(e)).sum();
    let disk = match held {
        0 => String::new(),
        bytes => format!(", {} still on disk", human_size(bytes)),
    };
    println!(
        "\n{} stopped, ports kept{disk}. {} still running.",
        doomed.len(),
        running.len() - doomed.len()
    );
    Ok(())
}

/// `down` reclaims CPU and, left to itself, says nothing about the gigabyte it leaves —
/// so a reader who never learns disk is separate never learns what frees it. The path is
/// repeated inside the hint on purpose: agents copy the command, not the line above it.
/// Silent when no service declares a setup, because then grove put nothing there.
fn kept_line(instance: &Instance) -> Option<String> {
    if !instance.config.services.iter().any(|s| s.setup.is_some()) {
        return None;
    }
    let worktree = instance.resolved.worktree.display();
    let held = match disk_of(&instance.entry) {
        Some(bytes) => format!(
            "{} on disk; git worktree remove {worktree} frees it",
            human_size(bytes)
        ),
        None => format!("dependencies on disk; git worktree remove {worktree} frees them"),
    };
    Some(format!("kept  {worktree}  ({held})"))
}

/// The stored figure, but only while the directory it describes exists. An orphan's
/// blocks went with its worktree, and reporting them would count disk already returned.
fn disk_of(entry: &Entry) -> Option<u64> {
    entry.disk_bytes.filter(|_| !entry.is_orphaned())
}

/// Stored private-dependency sizes and how many instances grove has not measured.
fn disk_footer(entries: &[Entry]) -> Option<String> {
    let measured: Vec<u64> = entries.iter().filter_map(disk_of).collect();
    let unmeasured = entries
        .iter()
        .filter(|e| !e.is_orphaned() && e.disk_bytes.is_none())
        .count();
    if measured.is_empty() && unmeasured == 0 {
        return None;
    }
    let mut line = if measured.is_empty() {
        "private-dependency footprint: none measured".to_string()
    } else {
        format!(
            "{} on disk in private dependencies across {} instances",
            human_size(measured.iter().sum()),
            measured.len()
        )
    };
    if unmeasured > 0 {
        line.push_str(&format!(", {unmeasured} unmeasured (grove up --footprint)"));
    }
    // The shared trees once, whatever the number of worktrees linking them — the point of
    // the store is that this number stops growing with the fleet. Read from what each
    // entry recorded, never walked: `ls` is a glance, and the walk took 89s on a 10G store.
    if let Ok(store) = grove::store::dir()
        && let Ok(size) = grove::store::size(&store)
        && size.entries > 0
    {
        if size.unmeasured < size.entries {
            line.push_str(&format!(", store {}", human_size(size.bytes)));
        }
        if size.unmeasured > 0 {
            line.push_str(&format!(
                ", {} store {} unmeasured until next linked",
                size.unmeasured,
                if size.unmeasured == 1 {
                    "entry"
                } else {
                    "entries"
                }
            ));
        }
    }
    Some(line)
}

/// The machine-wide counterpart to `status --json`: everything an agent needs to decide
/// whether the box is the problem, without parsing a table or shelling out to `uptime`.
/// From the run log alone: `ls` reads the registry and has no config to check setup with.
fn last_up(entry: &Entry) -> Option<grove::last_up::LastUp> {
    entry.instance_dir.as_deref().and_then(grove::last_up::read)
}

fn ls_json(entries: &[Entry], now: u64, machine: &Machine) -> serde_json::Value {
    let instances: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "slug": e.slug,
                "worktree": e.worktree,
                "running": e.is_running(),
                "orphaned": e.is_orphaned(),
                "idle_seconds": e.idle_seconds(now),
                "exposed": e.exposure.is_exposed(),
                "public_host": e.exposure.public_host(),
                "ports": e.ports,
                "database": e.db_name,
                "started_by": e.started_by,
                "disk_bytes": disk_of(e),
                "last_up_failed": last_up(e).is_some_and(|last| last.failed()),
                "last_up_interrupted": last_up(e).is_some_and(|last| last.interrupted()),
                "up_running": last_up(e).is_some_and(|last| last.running()),
            })
        })
        .collect();
    let disk: Vec<u64> = entries.iter().filter_map(disk_of).collect();

    serde_json::json!({
        "load": machine.load.as_ref().map(|l| l.one),
        "cores": machine.load.as_ref().map(|l| l.cores),
        "running": machine.running,
        "crowded": machine.crowded(),
        "disk_bytes": (!disk.is_empty()).then(|| disk.iter().sum::<u64>()),
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
    let starting = instance.last_up().is_some_and(|last| last.running());
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
            (false, _) if starting => "not started yet (grove up is running)".to_string(),
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
        "setup_incomplete": instance.setup_incomplete(),
        "last_up": instance.last_up().map(|last| serde_json::json!({
            "failed": last.failed(),
            "interrupted": last.interrupted(),
            "running": last.running(),
            "phase": last.phase,
            "exit_code": last.exit_code(),
            "error": last.error(),
            "log": last.log,
        })),
    })
}

/// `status` for an instance whose worktree is gone or left behind as a husk: what the
/// registry still holds for it, and the one command that clears it.
fn print_orphan_status(entry: &Entry, prune: &str) {
    println!("instance  {}", entry.slug);
    println!(
        "orphaned  worktree {} {}",
        entry.worktree.display(),
        entry
            .orphaned()
            .map_or_else(String::new, |orphan| orphan.to_string())
    );
    if !entry.ports.is_empty() {
        let ports: Vec<String> = entry
            .ports
            .iter()
            .map(|(name, port)| format!("{name}={port}"))
            .collect();
        println!("reserved  {}", ports.join(" "));
    }
    if let Some(db) = &entry.db_name {
        println!("database  {db}");
    }
    let live: Vec<&str> = entry
        .services
        .iter()
        .filter(|(_, handle)| grove::supervise::is_alive(handle))
        .map(|(name, _)| name.as_str())
        .collect();
    if !live.is_empty() {
        println!("running   {}", live.join(" "));
    }
    println!();
    println!("  {prune}   # stops what it left running and forgets the instance");
}

fn orphan_status_json(entry: &Entry, prune: &str) -> serde_json::Value {
    let services: serde_json::Map<String, serde_json::Value> = entry
        .services
        .iter()
        .map(|(name, handle)| {
            (
                name.clone(),
                serde_json::json!({ "running": grove::supervise::is_alive(handle) }),
            )
        })
        .collect();
    serde_json::json!({
        "slug": entry.slug,
        "worktree": entry.worktree,
        "orphaned": true,
        "orphan_reason": entry.orphaned().map(|orphan| orphan.to_string()),
        "fix": prune,
        "ports": entry.ports,
        "database": entry.db_name,
        "services": services,
    })
}

/// Why an instance that looks merely stopped is not usable: a setup that never finished,
/// or an `up` that failed before it could start anything. Silent when all is well.
fn attention(instance: &Instance) -> Vec<String> {
    let incomplete = instance.setup_incomplete();
    let last = instance.last_up();
    let mut notes = Vec::new();
    // Before the setup check: mid-run, setup is incomplete by definition, and telling
    // someone to run `grove up` while one is working sends them to start a second.
    if let Some(running) = last.as_ref().filter(|last| last.running()) {
        notes.push(format!(
            "up:       {}; wait for it, do not start another",
            running.describe()
        ));
        if let Some(phase) = &running.phase {
            notes.push(format!("now:      {phase}"));
        }
    } else if !incomplete.is_empty() {
        let why = match &last {
            Some(last) => last.describe(),
            None => "no grove up has run here".to_string(),
        };
        notes.push(format!(
            "setup: {} never completed ({why}); run grove up",
            incomplete.join(", ")
        ));
    } else if let Some(last) = last.as_ref().filter(|last| last.failed()) {
        notes.push(format!("{} (failed)", last.describe()));
    } else if let Some(last) = last.as_ref().filter(|last| last.interrupted()) {
        notes.push(last.describe());
    }
    if let Some(last) = last.filter(|_| !notes.is_empty()) {
        notes.push(format!("up log:   {}", last.log.display()));
    }
    notes
}

/// The only thing an agent should need to read after `up`.
/// The worktree to `cd` into when status was asked about an instance from outside it.
/// The hints that follow act on the current worktree, so without this they would act
/// on the wrong one, or fail.
fn elsewhere<'a>(instance: &'a Instance, cwd: &Path) -> Option<&'a Path> {
    let worktree = &instance.resolved.worktree;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    (!canon(cwd).starts_with(canon(worktree))).then_some(worktree.as_path())
}

fn print_summary(instance: &Instance, cd: Option<&Path>, notes: &[String]) {
    println!("instance  {}", instance.resolved.slug);
    for note in notes {
        println!("{note}");
    }
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
    if let Some(worktree) = cd {
        println!("  cd {}", worktree.display());
    }
    println!("  grove run -- <your test command>");
    println!("  grove logs {first} -f");
}

fn setup_outcome_line(
    service: &grove::config::Service,
    outcome: grove::instance::SetupOutcome,
) -> String {
    use grove::instance::SetupOutcome;
    let cache_path = service.cache.as_ref().map(|cache| cache.path.as_str());
    match outcome {
        SetupOutcome::Skipped => format!("{}: setup up to date, nothing to do", service.name),
        SetupOutcome::Installed => {
            format!("{}: ran setup (no grove dependency cache)", service.name)
        }
        SetupOutcome::Linked { hash } => format!(
            "{}: cache hit, linked {} from store ({})",
            service.name,
            cache_path.unwrap_or("dependencies"),
            &hash[..hash.len().min(8)]
        ),
        SetupOutcome::Stored { hash } => format!(
            "{}: cache miss, installed and stored {} ({})",
            service.name,
            cache_path.unwrap_or("dependencies"),
            &hash[..hash.len().min(8)]
        ),
    }
}

fn setup_plan_line(name: &str, plan: &grove::instance::SetupInstallPlan) -> String {
    use grove::instance::SetupInstallPlan;
    match plan {
        SetupInstallPlan::NoCache => format!("{name}: will run setup (no grove dependency cache)"),
        SetupInstallPlan::LinkOnly | SetupInstallPlan::AlreadyInPlace => {
            format!("{name}: cache hit (link-only)")
        }
        SetupInstallPlan::FullInstall(_) => format!("{name}: cache miss (full install)"),
    }
}
