//! How busy the machine is, and whether that is worth saying out loud.
//!
//! grove starts one dev stack per worktree and nothing creates pressure to stop them, so
//! a machine can quietly end up running a dozen. The cost does not show up as a grove
//! failure — it shows up as tests timing out on a branch that did not break them.

/// The one-minute load average against the number of cores available to carry it.
pub struct Load {
    pub one: f64,
    pub cores: usize,
}

impl Load {
    /// More runnable work than cores to run it. Measured against core count rather than a
    /// fixed number: load 26 is a crisis on a laptop and a quiet afternoon on a build box.
    pub fn oversubscribed(&self) -> bool {
        self.one >= self.cores as f64
    }
}

/// Below this many running instances, a loaded machine is somebody's type-check rather
/// than a pile-up grove can do anything about. Warning there costs more than it saves:
/// the reader learns to skip grove's warnings, including the one that mattered.
const CROWD: usize = 4;

/// Whether the machine's state is worth mentioning. Both halves are required — a loaded
/// machine with nothing reclaimable leaves the reader with no move to make, and a crowd
/// on a machine that is coping fine is not a problem yet.
pub fn should_warn(load: Option<&Load>, running: usize) -> bool {
    running >= CROWD && load.is_some_and(Load::oversubscribed)
}

/// The machine's current load, or None if it cannot be read.
pub fn sample() -> Option<Load> {
    let one = match override_value("GROVE_LOAD") {
        Some(forced) => forced,
        None => machine_load()?,
    };
    let cores = override_value("GROVE_CORES")
        .map(|c| c as usize)
        .unwrap_or_else(cores)
        .max(1);
    Some(Load { one, cores })
}

/// Real machine load is not something a test can arrange, so both halves can be forced.
/// Same escape hatch `GROVE_STATE_DIR` already provides for the registry — deliberately
/// undocumented, because it is a test seam rather than configuration.
fn override_value(name: &str) -> Option<f64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn machine_load() -> Option<f64> {
    let mut averages = [0f64; 3];
    // SAFETY: getloadavg writes at most the requested number of elements, and it is
    // handed the length of the array it is writing into.
    let filled = unsafe { libc::getloadavg(averages.as_mut_ptr(), 3) };
    (filled > 0).then_some(averages[0])
}

fn cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
