# Seed snapshot design, parked

Status: measurement only. Revisit after the macOS clone soak week. Grove has no snapshot implementation. This note does not approve one.

## Measured opportunity

On this 10-core Mac at load 43-54, the base seed took 14.903 seconds and the demo seed took 17.337 seconds. Three dump runs took 0.425, 0.214 and 0.158 seconds. Three restores took 1.821, 1.551 and 1.476 seconds.

The archive held 78 documents across 41 collections in 80,444 bytes. Each restore used a new database name and matched source document hashes and index definitions. These warm runs on a small fixture suggest about 30 seconds saved per new worktree. They exclude startup, validation and cache management.

Source was Mondrio develop `63acaad242`, including the offline membership fixture. An earlier attempt used stale main-checkout files and failed on a WorkOS request. The probe removed its databases and temporary worktree.

Evidence on the Mac: `/tmp/grove_snapshot_probe_d04e5c3f1b/`, including `results.json`, `field-audit.json`, the probe, seed source copies and archive.

## Canonical offline fixture

Build only in a new, isolated database from the target worktree's code. Use the base harness seed followed by the demo seed, dummy test settings and blocked external connections. Never take a snapshot from a lane database that served real work.

The measured fixture includes a sent quote, its approval, a share link, email history and migration records. It contains fixed test identities and generated internal IDs. Restores must keep database namespaces isolated and preserve indexes. Application acceptance on a restored database remains untested.

## Cache identity

A command hash alone is insufficient. A proposed identity must cover:

- Both seed commands and their source, including the currently untracked demo script.
- Application code, models, migrations, index definitions and the offline harness.
- Dependency lockfiles, the snapshot format and relevant MongoDB/tool versions.
- A declared test-settings profile, feature flags and provider modes.
- The freshness policy and any external fixture artifacts.

Define an explicit manifest before implementation. Do not put live secrets in the manifest or use undeclared inherited settings. Start conservatively with a backend source-tree digest rather than guessing which transitive imports matter.

## Freshness and expiry

Proposed starting policy, still subject to review: rebuild at least daily and reject an archive before any required fixture expires. Never silently rewrite dates during restore. Dates participate in quote, FX and audit semantics.

The sample's quote expires on 2026-11-01. Creation, publication, send and audit times stay frozen. TTL indexes remain active after restore. Its share-link expiry, password expiry and portal host are null. Test the fixture's usable lifetime explicitly before the age-limit decision.

## Portal-code output contract

The database stores `code_sha256`. Seed output holds the raw portal code. A database-only archive cannot reproduce that output.

Before implementation, choose either a protected test-only output manifest that preserves and verifies the code, or an explicit per-restore step that mints a new code. Do not describe a code from archived output as fresh. Keep live buyer credentials out of snapshots. Decide how links behave across isolated databases, hosts and test-secret rotations.

## Revisit gate

After the clone soak week, settle the manifest, expiry and output contracts. Then test a restored fixture through the application and compare complete startup time. Grove 0.1.25 contains no snapshot implementation.
