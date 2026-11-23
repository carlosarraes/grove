//! `grove top` attribution and the shared idle definition, from fixture `ps` output.

use grove::registry::Entry;
use grove::supervise::Handle;
use grove::top::{Activity, Class, Cwds, Snapshot, classify, parse_ps, report};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

const NOW: u64 = 1_800_000_000;

fn entry(slug: &str, services: &[(&str, u32)], last_used_ago: u64) -> Entry {
    Entry {
        worktree: PathBuf::from(format!("/wt/{slug}")),
        slug: slug.to_string(),
        ports: BTreeMap::new(),
        services: services
            .iter()
            .map(|(name, pid)| {
                (
                    name.to_string(),
                    Handle {
                        pid: *pid,
                        started_at: NOW - 3600,
                    },
                )
            })
            .collect(),
        db_name: None,
        db_resource: None,
        exposure: Default::default(),
        last_used: Some(NOW - last_used_ago),
        instance_dir: None,
        disk_bytes: None,
        started_by: Some("herdr pane 7".into()),
        cache_keys: BTreeMap::new(),
        released: false,
    }
}

/// Linux rows (procps: right-aligned, `=` suffix, no header) and macOS rows (header,
/// comma decimals under a pt_BR locale, `.app` bundles with spaces) in one listing.
const FIXTURE: &str = "\
  PID  PPID  PGID  %CPU    RSS COMMAND
      1       0       1  0.0 11588 /sbin/init
    100       1     100  0.0  2000 sh -c uvicorn app:main
    101     100     100 40.5 300000 /wt/busy/.venv/bin/python -m uvicorn app:main
    102     101     102 10.0 100000 /usr/bin/esbuild --service
    200       1     200  5.0 50000 sh -c npm run dev
    300       1     300  1,0  4000 sh -c sleep 1000
    310       1     300  7,5 70000 node /wt/reaped/node_modules/.bin/vite
    311     310     311  2,5 30000 /usr/bin/esbuild --service
    400       1     400 90.0 900000 node /wt/busy/node_modules/vitest/dist/workers/forks.js
    401       1     401 50.0 500000 claude --effort medium
    402     401     401 20.0 200000 node /home/u/.npm/mcp-server.js
    403       1     403 30.0 300000 /usr/bin/python3 -c from multiprocessing.spawn import spawn_main
    404       1     404  3.0  9000 /bin/zsh
    500       1     500 60.0 600000 /wt/quiet/.venv/bin/pytest -n 4
    501     500     500 45.0 400000 /usr/bin/python3 -c import sys;exec(eval(sys.stdin.readline()))
    600       1     600  0.4 250000 claude
    700       1     700  0.0  1000 /bin/zsh
    800       1     800 383,7 2242000 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome
    801     800     800 100.0 500000 /Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer) --type=renderer
    900       1     900 40.8 3875000 /System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine
    950       1     950 12.0 800000 /Applications/T3 Code.app/Contents/MacOS/T3 Code
    960       1     960  1.0 10000 /usr/sbin/cron
";

fn entries() -> Vec<Entry> {
    vec![
        // Running: backend group 100 (with esbuild that left the group), web group 200
        // has no live process, so only backend is live.
        entry("busy", &[("backend", 100), ("web", 99999)], 600),
        // Running on a reaped leader: group 300's leader is gone, its vite survives.
        entry("reaped", &[("frontend", 300)], 60),
        // Stopped, but its pytest is still burning CPU.
        entry("quiet", &[], 9 * 3600),
        // Stopped with a leftover agent (cwd only).
        entry("leftover", &[], 9 * 3600),
        // Stopped and silent.
        entry("silent", &[], 9 * 3600),
    ]
}

fn snapshot() -> Snapshot {
    let mut snapshot = Snapshot::from_ps(FIXTURE);
    snapshot.cwds = Cwds::Fixed(HashMap::from([
        (401, PathBuf::from("/wt/busy/backend")),
        (403, PathBuf::from("/wt/busy")),
        (404, PathBuf::from("/wt/busy")),
        (600, PathBuf::from("/wt/leftover")),
        (700, PathBuf::from("/wt/silent")),
    ]));
    snapshot
}

#[test]
fn parses_linux_and_macos_rows_alike() {
    let procs = parse_ps(FIXTURE);
    assert_eq!(procs.len(), 22, "the header is skipped, every row is kept");
    let chrome = procs.iter().find(|p| p.pid == 800).unwrap();
    assert_eq!(chrome.cpu, 383.7, "a comma decimal is a decimal");
    assert_eq!(chrome.rss_kb, 2_242_000);
    assert!(
        chrome
            .command
            .ends_with("Google Chrome.app/Contents/MacOS/Google Chrome")
    );
    let esbuild = procs.iter().find(|p| p.pid == 102).unwrap();
    assert_eq!((esbuild.ppid, esbuild.pgid), (101, 102));
}

#[test]
fn services_sum_their_process_group_and_reach_children_that_left_it() {
    let report = report(&snapshot(), &entries(), NOW, false);
    let busy = report.rows.iter().find(|r| r.slug == "busy").unwrap();

    assert_eq!(busy.services.len(), 1, "web has no live process: {busy:?}");
    let backend = &busy.services[0];
    assert_eq!(backend.name, "backend");
    assert_eq!(
        backend.usage.processes, 3,
        "sh, uvicorn, and esbuild via the pid tree"
    );
    assert!((backend.usage.cpu - 50.5).abs() < 1e-9, "{backend:?}");
    assert_eq!(busy.uptime_seconds, Some(3600));
    assert_eq!(busy.last_command_seconds, Some(600));
}

#[test]
fn a_reaped_leader_keeps_its_group_and_the_groups_children() {
    let report = report(&snapshot(), &entries(), NOW, false);
    let reaped = report.rows.iter().find(|r| r.slug == "reaped").unwrap();
    let frontend = &reaped.services[0];
    assert_eq!(
        frontend.usage.processes, 3,
        "sleep and vite by group 300, esbuild by vite's tree: {frontend:?}"
    );
    assert!(
        reaped.other.processes == 0,
        "vite's argv names the worktree, but the service claimed it first"
    );
}

#[test]
fn rows_split_tests_agents_and_other_and_sort_by_total_cpu() {
    let report = report(&snapshot(), &entries(), NOW, false);
    let slugs: Vec<&str> = report.rows.iter().map(|r| r.slug.as_str()).collect();
    assert_eq!(slugs, ["busy", "reaped"], "running only, heaviest first");

    let busy = &report.rows[0];
    assert_eq!(busy.tests.processes, 1, "vitest by argv: {busy:?}");
    assert_eq!(busy.tests.cpu, 90.0);
    assert_eq!(
        busy.agents.processes, 2,
        "claude by cwd, its MCP server by parent"
    );
    assert_eq!(busy.agents.cpu, 70.0);
    assert_eq!(
        busy.other.processes, 2,
        "a python worker and a shell, by cwd"
    );
    assert!((busy.cpu - (50.5 + 90.0 + 70.0 + 33.0)).abs() < 1e-9);
}

#[test]
fn all_adds_stopped_instances_with_tests_or_agents_but_not_quiet_ones() {
    let report = report(&snapshot(), &entries(), NOW, true);
    let slugs: Vec<&str> = report.rows.iter().map(|r| r.slug.as_str()).collect();
    assert_eq!(slugs, ["busy", "quiet", "reaped", "leftover"]);

    let quiet = &report.rows[1];
    assert!(!quiet.running);
    assert_eq!(
        quiet.tests.processes, 2,
        "pytest and its xdist worker by parent"
    );
    let leftover = &report.rows[3];
    assert_eq!(leftover.agents.processes, 1);
    assert!(
        !slugs.contains(&"silent"),
        "a shell sitting in a stopped worktree is not a leftover"
    );
}

#[test]
fn the_footer_names_the_heaviest_programs_outside_every_worktree() {
    let report = report(&snapshot(), &entries(), NOW, true);
    let names: Vec<&str> = report.outside.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Google Chrome",
            "com.apple.Virtualization.VirtualMachine",
            "T3 Code"
        ],
        "helpers fold into their bundle; vitest and pytest are in rows, not here"
    );
    assert!((report.outside[0].usage.cpu - 483.7).abs() < 1e-9);
    assert_eq!(report.outside[0].usage.processes, 2);
}

#[test]
fn classify_by_argv() {
    for (command, class) in [
        ("claude --effort medium", Class::Agents),
        (
            "node /usr/lib/node_modules/@openai/codex/bin/codex.js",
            Class::Agents,
        ),
        ("claude -p run pytest please", Class::Agents),
        ("/wt/x/.venv/bin/python -m pytest -x", Class::Tests),
        ("node /wt/x/node_modules/.bin/tsc --noEmit", Class::Tests),
        ("node (vitest 3)", Class::Tests),
        (
            "/home/u/.agent-browser/browsers/chrome/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            Class::Tests,
        ),
        (
            "node /wt/x/node_modules/.bin/vite --port 3000",
            Class::Other,
        ),
        ("/bin/zsh", Class::Other),
    ] {
        assert_eq!(classify(command), class, "{command}");
    }
}

#[test]
fn tests_or_agents_in_a_worktree_mean_it_is_not_idle() {
    let activity = Activity::from_snapshot(&snapshot(), &entries());
    let all = entries();
    let busy = &all[0];
    assert_eq!(
        busy.idle_seconds(NOW),
        Some(600),
        "the clock itself is untouched"
    );
    assert_eq!(
        busy.effective_idle(NOW, &activity),
        None,
        "vitest is running here"
    );
    assert_eq!(
        all[2].effective_idle(NOW, &activity),
        None,
        "pytest is running here"
    );
    assert_eq!(
        all[3].effective_idle(NOW, &activity),
        None,
        "an agent is here"
    );
}

#[test]
fn service_processes_alone_do_not_make_a_stack_active() {
    // `reaped` runs only its services, and a shell sits in `silent`.
    let mut all = entries();
    all[1].last_used = Some(NOW - 5 * 3600);
    let activity = Activity::from_snapshot(&snapshot(), &all);
    assert_eq!(all[1].effective_idle(NOW, &activity), Some(5 * 3600));
    assert_eq!(all[4].effective_idle(NOW, &activity), Some(9 * 3600));
}
