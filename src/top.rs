//! Who is spending the machine's CPU: per instance, and what runs outside every worktree.
//!
//! Read-only. It never signals a process and never writes the registry; it reads one `ps`
//! snapshot and attributes each row to an instance by service process group first, then
//! by a worktree path in its argv, then by its working directory, then by its parent.
//!
//! The same attribution answers the idle question: a worktree with live tests or agents
//! is in use whatever its last grove command says ([`Activity`]).

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::footprint::human_size;
use crate::registry::{Entry, human_age};

/// One `ps` row. `cpu` is percent of one core; `rss_kb` is resident kilobytes on both
/// platforms.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub cpu: f64,
    pub rss_kb: u64,
    pub command: String,
}

/// Rows of `ps -axo pid,ppid,pgid,pcpu,rss,command`, with or without a header.
/// Lines that do not start with five numbers are skipped. A comma decimal separator
/// (macOS under a non-C locale) is accepted.
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line.trim_start();
            let mut field = || {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let (value, tail) = rest.split_at(end);
                rest = tail.trim_start();
                value
            };
            let pid = field().parse().ok()?;
            let ppid = field().parse().ok()?;
            let pgid = field().parse().ok()?;
            let cpu = field().replace(',', ".").parse().ok()?;
            let rss_kb = field().parse().ok()?;
            Some(Proc {
                pid,
                ppid,
                pgid,
                cpu,
                rss_kb,
                command: rest.trim_end().to_string(),
            })
        })
        .collect()
}

/// Every process below `root`, parents before children, given (pid, ppid) pairs.
pub fn descendants_of(pairs: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut found = vec![root];
    let mut index = 0;
    while index < found.len() {
        let parent = found[index];
        found.extend(
            pairs
                .iter()
                .filter(|(pid, ppid)| *ppid == parent && *pid != parent)
                .map(|(pid, _)| *pid),
        );
        index += 1;
    }
    found.remove(0);
    found
}

/// Where a process's working directory comes from.
#[derive(Debug, Clone)]
pub enum Cwds {
    /// Not looked up: a fixture's pids name no real process.
    None,
    /// `/proc/<pid>/cwd`, cheap enough to read for every unattributed process.
    Proc,
    /// One batched `/usr/sbin/lsof`, only for processes that look like tests or agents.
    Lsof,
    /// Given, for tests.
    Fixed(HashMap<u32, PathBuf>),
}

pub struct Snapshot {
    pub procs: Vec<Proc>,
    pub cwds: Cwds,
    /// This process and anything it spawned, which is never anyone's load.
    pub own: u32,
}

/// Whether CPU must be measured over an interval or can be read as `ps` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cpu {
    /// Membership only (the idle check): no sleep.
    Ignore,
    /// Recent CPU (`top`): macOS `pcpu` already is; Linux measures ticks over `SAMPLE`.
    Recent,
}

const SAMPLE: Duration = Duration::from_millis(1000);

impl Snapshot {
    /// A fixture snapshot: no cwd lookups, CPU as written.
    pub fn from_ps(text: &str) -> Self {
        Snapshot {
            procs: parse_ps(text),
            cwds: Cwds::None,
            own: 0,
        }
    }

    /// The machine now. `GROVE_PS_FIXTURE` names a file of `ps` output to read instead —
    /// a test seam like `GROVE_LOAD`, deliberately undocumented.
    pub fn take(cpu: Cpu) -> Result<Self> {
        if let Some(path) = std::env::var_os("GROVE_PS_FIXTURE").filter(|p| !p.is_empty()) {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", Path::new(&path).display()))?;
            return Ok(Snapshot::from_ps(&text));
        }
        let linux = cfg!(target_os = "linux");
        let before = (linux && cpu == Cpu::Recent).then(|| {
            let ticks = proc_ticks(all_proc_pids());
            std::thread::sleep(SAMPLE);
            ticks
        });
        let mut procs = parse_ps(&ps()?);
        if let Some((start, ticks0)) = before {
            let (end, ticks1) = proc_ticks(procs.iter().map(|p| p.pid));
            let elapsed = end.duration_since(start).as_secs_f64().max(0.001);
            let hz = clock_ticks();
            for proc in &mut procs {
                proc.cpu = match ticks1.get(&proc.pid) {
                    Some(now) => {
                        let then = ticks0.get(&proc.pid).copied().unwrap_or(0);
                        now.saturating_sub(then) as f64 / hz / elapsed * 100.0
                    }
                    None => 0.0,
                };
            }
        }
        Ok(Snapshot {
            procs,
            cwds: if linux { Cwds::Proc } else { Cwds::Lsof },
            own: std::process::id(),
        })
    }
}

fn ps() -> Result<String> {
    let output = Command::new("ps")
        .args(["-ww", "-axo", "pid=,ppid=,pgid=,pcpu=,rss=,command="])
        .env("LC_ALL", "C")
        .output()
        .context("running ps")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn all_proc_pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

/// utime + stime per pid, and when they were read.
fn proc_ticks(pids: impl IntoIterator<Item = u32>) -> (Instant, HashMap<u32, u64>) {
    let at = Instant::now();
    let ticks = pids
        .into_iter()
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // The command name may contain spaces and parentheses; fields resume after
            // the last `)`, starting at field 3 (state). utime and stime are 14 and 15.
            let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            Some((pid, utime + stime))
        })
        .collect();
    (at, ticks)
}

fn clock_ticks() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as f64 } else { 100.0 }
}

/// What a process outside every service group is doing in a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Tests,
    Agents,
    Other,
}

const TEST_NAMES: &[&str] = &[
    "pytest",
    "py.test",
    "vitest",
    "jest",
    "jest-worker",
    "tsc",
    "playwright",
    "ms-playwright",
    "chrome-headless-shell",
];

/// Tests or agents by argv alone. Agents first and only by the program name, because an
/// agent's prompt may well mention pytest.
pub fn classify(command: &str) -> Class {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let base = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    if tokens
        .iter()
        .take(2)
        .map(|t| base(t))
        .any(|name| name == "claude" || name == "codex" || name.starts_with("codex-"))
        || command.contains("@anthropic-ai/claude-code")
        || command.contains("@openai/codex")
    {
        return Class::Agents;
    }
    let trim = |c: &str| {
        c.trim_matches(|ch: char| matches!(ch, '(' | ')' | '"' | '\'' | ','))
            .to_string()
    };
    let named_test = tokens.iter().any(|token| {
        token
            .split(['/', '='])
            .map(trim)
            .any(|part| TEST_NAMES.contains(&part.as_str()))
    });
    if named_test || command.contains("Chrome for Testing") {
        Class::Tests
    } else {
        Class::Other
    }
}

/// Who a process belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Service(String),
    Worktree(Class),
}

/// Each attributed pid's instance (by index into the entries given) and role.
pub struct Attribution {
    pub by_pid: HashMap<u32, (usize, Owner)>,
    /// This process and its children.
    pub own: HashSet<u32>,
}

pub fn attribute(snapshot: &Snapshot, entries: &[Entry]) -> Attribution {
    let procs = &snapshot.procs;
    let pairs: Vec<(u32, u32)> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut by_pid: HashMap<u32, (usize, Owner)> = HashMap::new();

    let own: HashSet<u32> = if snapshot.own == 0 {
        HashSet::new()
    } else {
        std::iter::once(snapshot.own)
            .chain(descendants_of(&pairs, snapshot.own))
            .collect()
    };

    // Services: `process_group(0)` makes the group id the recorded leader pid. A child
    // that left the group (setsid, a browser) is reached by walking the pid tree from
    // every member, the leader included while it lives and the survivors once it is
    // reaped. Walking only from group members keeps a reused pid from claiming a tree.
    for (index, entry) in entries.iter().enumerate() {
        for (name, handle) in &entry.services {
            let mut members: Vec<u32> = procs
                .iter()
                .filter(|p| p.pgid == handle.pid)
                .map(|p| p.pid)
                .collect();
            for root in members.clone() {
                members.extend(descendants_of(&pairs, root));
            }
            for pid in members {
                by_pid
                    .entry(pid)
                    .or_insert_with(|| (index, Owner::Service(name.clone())));
            }
        }
    }

    let worktrees: HashMap<&Path, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.worktree.as_path(), i))
        .collect();
    let lookup = |path: &Path| path.ancestors().find_map(|a| worktrees.get(a).copied());

    let free = |pid: u32, by_pid: &HashMap<u32, (usize, Owner)>| {
        !own.contains(&pid) && !by_pid.contains_key(&pid)
    };

    // argv: any absolute path token at or under a worktree.
    let mut unmatched = Vec::new();
    for proc in procs {
        if !free(proc.pid, &by_pid) {
            continue;
        }
        let hit = proc
            .command
            .split(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | '"' | '\'' | ','))
            .filter(|t| t.starts_with('/'))
            .find_map(|t| lookup(Path::new(t)));
        match hit {
            Some(index) => {
                by_pid.insert(proc.pid, (index, Owner::Worktree(classify(&proc.command))));
            }
            None => unmatched.push(proc),
        }
    }

    // cwd for the rest.
    let cwds = cwds_of(&snapshot.cwds, &unmatched);
    for proc in unmatched {
        if let Some(index) = cwds.get(&proc.pid).and_then(|cwd| lookup(cwd)) {
            by_pid.insert(proc.pid, (index, Owner::Worktree(classify(&proc.command))));
        }
    }

    // Children of tests and agents: pytest-xdist workers, a browser under Playwright, an
    // agent's tool servers. Not children of anything else — a terminal multiplexer
    // started in one worktree would otherwise claim every agent on the machine.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    let class_of: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.command.as_str())).collect();
    let mut queue: VecDeque<u32> = by_pid
        .iter()
        .filter(|(_, (_, owner))| matches!(owner, Owner::Worktree(Class::Tests | Class::Agents)))
        .map(|(pid, _)| *pid)
        .collect();
    while let Some(parent) = queue.pop_front() {
        let Some((index, Owner::Worktree(inherited))) = by_pid.get(&parent).cloned() else {
            continue;
        };
        for &child in children.get(&parent).into_iter().flatten() {
            if child == parent || !free(child, &by_pid) {
                continue;
            }
            let own_class = class_of
                .get(&child)
                .map(|c| classify(c))
                .unwrap_or(Class::Other);
            let class = if own_class == Class::Other {
                inherited
            } else {
                own_class
            };
            by_pid.insert(child, (index, Owner::Worktree(class)));
            queue.push_back(child);
        }
    }

    Attribution { by_pid, own }
}

fn cwds_of(source: &Cwds, procs: &[&Proc]) -> HashMap<u32, PathBuf> {
    match source {
        Cwds::None => HashMap::new(),
        Cwds::Fixed(map) => map.clone(),
        Cwds::Proc => procs
            .iter()
            .filter_map(|p| {
                Some((
                    p.pid,
                    std::fs::read_link(format!("/proc/{}/cwd", p.pid)).ok()?,
                ))
            })
            .collect(),
        Cwds::Lsof => {
            // Only processes that would matter: a shell sitting in a worktree is not
            // worth an lsof, an agent whose argv is just `claude` is.
            let pids: Vec<String> = procs
                .iter()
                .filter(|p| classify(&p.command) != Class::Other)
                .map(|p| p.pid.to_string())
                .collect();
            if pids.is_empty() {
                return HashMap::new();
            }
            // PATH may lack /usr/sbin under ssh or launchd.
            let lsof = if Path::new("/usr/sbin/lsof").exists() {
                "/usr/sbin/lsof"
            } else {
                "lsof"
            };
            Command::new(lsof)
                .args(["-a", "-d", "cwd", "-Fn", "-p", &pids.join(",")])
                .output()
                .map(|out| crate::free::parse_lsof_cwds(&String::from_utf8_lossy(&out.stdout)))
                .unwrap_or_default()
                .into_iter()
                .collect()
        }
    }
}

/// Worktrees with live tests or agents in them: the one definition of activity beyond
/// grove commands, shared by `ls`, `health`, `down --idle` and the `up`/`run` sweep.
/// Service processes never count (see [`Entry::idle_seconds`]).
#[derive(Debug, Default)]
pub struct Activity {
    busy: HashSet<PathBuf>,
}

impl Activity {
    /// Best effort: a machine where `ps` fails reports no activity, which is what grove
    /// assumed before this existed.
    pub fn sample(entries: &[Entry]) -> Self {
        match Snapshot::take(Cpu::Ignore) {
            Ok(snapshot) => Activity::from_snapshot(&snapshot, entries),
            Err(_) => Activity::default(),
        }
    }

    pub fn from_snapshot(snapshot: &Snapshot, entries: &[Entry]) -> Self {
        let busy = attribute(snapshot, entries)
            .by_pid
            .values()
            .filter(|(_, owner)| matches!(owner, Owner::Worktree(Class::Tests | Class::Agents)))
            .map(|(index, _)| entries[*index].worktree.clone())
            .collect();
        Activity { busy }
    }

    pub fn is_busy(&self, worktree: &Path) -> bool {
        self.busy.contains(worktree)
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Usage {
    pub cpu: f64,
    pub rss_bytes: u64,
    pub processes: usize,
}

impl Usage {
    fn add(&mut self, proc: &Proc) {
        self.cpu += proc.cpu;
        self.rss_bytes += proc.rss_kb * 1024;
        self.processes += 1;
    }

    fn cell(&self) -> String {
        if self.processes == 0 {
            "-".to_string()
        } else {
            format!("{:.0}% {}", self.cpu, human_size(self.rss_bytes))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceUsage {
    pub name: String,
    pub pid: u32,
    #[serde(flatten)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub slug: String,
    pub worktree: PathBuf,
    pub running: bool,
    pub cpu: f64,
    pub rss_bytes: u64,
    /// Live services only.
    pub services: Vec<ServiceUsage>,
    pub tests: Usage,
    pub agents: Usage,
    pub other: Usage,
    /// Since the oldest live service started, from the recorded start time.
    pub uptime_seconds: Option<u64>,
    /// Since the last grove command; not an idle verdict.
    pub last_command_seconds: Option<u64>,
    pub started_by: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Consumer {
    pub name: String,
    #[serde(flatten)]
    pub usage: Usage,
}

pub struct Report {
    pub rows: Vec<Row>,
    /// The heaviest processes no instance accounts for, grouped by program.
    pub outside: Vec<Consumer>,
}

/// Rows for running instances (and with `all`, stopped ones still holding tests or
/// agents), heaviest first. Pure: no git, no load, no process lookups beyond the snapshot.
pub fn report(snapshot: &Snapshot, entries: &[Entry], now: u64, all: bool) -> Report {
    let attribution = attribute(snapshot, entries);
    let mut services: Vec<BTreeMap<String, Usage>> = vec![BTreeMap::new(); entries.len()];
    let mut buckets: Vec<[Usage; 3]> = vec![Default::default(); entries.len()];
    let mut outside: BTreeMap<String, Usage> = BTreeMap::new();

    for proc in &snapshot.procs {
        match attribution.by_pid.get(&proc.pid) {
            Some((index, Owner::Service(name))) => {
                services[*index].entry(name.clone()).or_default().add(proc)
            }
            Some((index, Owner::Worktree(class))) => buckets[*index][class.slot()].add(proc),
            None if attribution.own.contains(&proc.pid) => {}
            None => outside.entry(program(&proc.command)).or_default().add(proc),
        }
    }

    let mut rows: Vec<Row> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let live: Vec<ServiceUsage> = entry
                .services
                .iter()
                .filter_map(|(name, handle)| {
                    Some(ServiceUsage {
                        name: name.clone(),
                        pid: handle.pid,
                        usage: services[index].get(name)?.clone(),
                    })
                })
                .collect();
            let [tests, agents, other] = buckets[index].clone();
            let running = !live.is_empty();
            let leftover = tests.processes + agents.processes > 0;
            if !(running || all && leftover) {
                return None;
            }
            let uptime_seconds = entry
                .services
                .iter()
                .filter(|(name, _)| live.iter().any(|s| &s.name == *name))
                .map(|(_, h)| h.started_at)
                .filter(|at| *at > 0)
                .min()
                .map(|at| now.saturating_sub(at));
            let parts = live
                .iter()
                .map(|s| &s.usage)
                .chain([&tests, &agents, &other]);
            let (cpu, rss_bytes) = parts.fold((0.0, 0), |(c, r), u| (c + u.cpu, r + u.rss_bytes));
            Some(Row {
                slug: entry.slug.clone(),
                worktree: entry.worktree.clone(),
                running,
                cpu,
                rss_bytes,
                services: live,
                tests,
                agents,
                other,
                uptime_seconds,
                last_command_seconds: entry.idle_seconds(now),
                started_by: entry.started_by.clone(),
                branch: None,
            })
        })
        .collect();
    rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then_with(|| a.slug.cmp(&b.slug)));

    let mut outside: Vec<Consumer> = outside
        .into_iter()
        .map(|(name, usage)| Consumer { name, usage })
        .collect();
    outside.sort_by(|a, b| {
        b.usage
            .cpu
            .total_cmp(&a.usage.cpu)
            .then_with(|| a.name.cmp(&b.name))
    });
    outside.truncate(3);

    Report { rows, outside }
}

/// A name for a program that groups its helpers with it: the macOS bundle a binary lives
/// in ("Google Chrome" for every renderer), else the executable's file name.
pub fn program(command: &str) -> String {
    if let Some(bundle) = command
        .split(".app/")
        .next()
        .filter(|_| command.contains(".app/"))
    {
        return bundle.rsplit('/').next().unwrap_or(bundle).to_string();
    }
    let first = command.split_whitespace().next().unwrap_or(command);
    first
        .rsplit('/')
        .next()
        .unwrap_or(first)
        .trim_start_matches('-')
        .to_string()
}

impl Class {
    fn slot(self) -> usize {
        match self {
            Class::Tests => 0,
            Class::Agents => 1,
            Class::Other => 2,
        }
    }
}

fn branch(worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|b| !b.is_empty())
}

/// `grove top`.
pub fn run(all: bool, json: bool) -> Result<()> {
    let entries = crate::instance::registry()?.list()?;
    let snapshot = Snapshot::take(Cpu::Recent)?;
    let now = crate::registry::now();
    let mut report = report(&snapshot, &entries, now, all);
    // Only for rows that print: git on every registered worktree would cost seconds.
    for row in &mut report.rows {
        row.branch = branch(&row.worktree);
    }
    let load = crate::load::sample();

    if json {
        let value = serde_json::json!({
            "load": load.as_ref().map(|l| l.one),
            "cores": load.as_ref().map(|l| l.cores),
            "instances": report.rows,
            "outside_worktrees": report.outside,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    print!("{}", render(&report.rows));
    println!("{}", footer(load.as_ref(), &report.outside));
    Ok(())
}

pub fn render(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "no running instances\n".to_string();
    }
    let header = [
        "INSTANCE", "CPU", "RSS", "SERVICES", "TESTS", "AGENTS", "OTHER", "UP", "LAST CMD",
        "OWNER", "BRANCH",
    ];
    let mut table: Vec<Vec<String>> = vec![header.iter().map(|h| h.to_string()).collect()];
    for row in rows {
        let services = if row.services.is_empty() {
            "stopped".to_string()
        } else {
            row.services
                .iter()
                .map(|s| format!("{} {}", s.name, s.usage.cell()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        table.push(vec![
            row.slug.clone(),
            format!("{:.0}%", row.cpu),
            human_size(row.rss_bytes),
            services,
            row.tests.cell(),
            row.agents.cell(),
            row.other.cell(),
            row.uptime_seconds
                .map(human_age)
                .unwrap_or_else(|| "-".into()),
            row.last_command_seconds
                .map(|s| format!("{} ago", human_age(s)))
                .unwrap_or_else(|| "-".into()),
            row.started_by.clone().unwrap_or_else(|| "-".into()),
            row.branch.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|c| {
            table
                .iter()
                .map(|r| r[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for line in table {
        let cells: Vec<String> = line
            .iter()
            .enumerate()
            .map(|(c, cell)| match c {
                1 | 2 => format!("{cell:>width$}", width = widths[c]),
                _ if c == header.len() - 1 => cell.clone(),
                _ => format!("{cell:<width$}", width = widths[c]),
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

pub fn footer(load: Option<&crate::load::Load>, outside: &[Consumer]) -> String {
    let load = match load {
        Some(l) => format!("load {:.1} on {} cores", l.one, l.cores),
        None => "load unknown".to_string(),
    };
    if outside.is_empty() {
        return format!("\n{load}");
    }
    let named: Vec<String> = outside
        .iter()
        .map(|c| {
            format!(
                "{} {:.0}% {}",
                c.name,
                c.usage.cpu,
                human_size(c.usage.rss_bytes)
            )
        })
        .collect();
    format!("\n{load}; outside worktrees: {}", named.join(", "))
}
