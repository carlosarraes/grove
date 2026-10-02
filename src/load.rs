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

/// Admission is a load check, not a concurrency reservation. Keep fleetlock's slot cap.
pub fn wait_for_admission(config: &crate::config::Admission) -> anyhow::Result<()> {
    wait_with_samples(config, sample)
}

fn wait_with_samples(
    config: &crate::config::Admission,
    mut sample: impl FnMut() -> Option<Load>,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use std::time::{Duration, Instant};

    let timeout = crate::instance::parse_duration(&config.timeout)?;
    let began = Instant::now();
    let mut heartbeat = Duration::ZERO;
    loop {
        let elapsed = began.elapsed();
        if elapsed >= timeout {
            bail!(
                "admission: timed out after {:.1}s; command was not started",
                elapsed.as_secs_f64()
            );
        }
        let load = sample()
            .filter(|load| load.one.is_finite() && load.one >= 0.0)
            .context("admission: cannot read a valid load average; command was not started")?;
        let threshold = config.max_load.unwrap_or(load.cores as f64);
        if load.one < threshold {
            eprintln!(
                "admission: admitted (load {:.2}, threshold {:.2}, waited {:.1}s)",
                load.one,
                threshold,
                elapsed.as_secs_f64()
            );
            return Ok(());
        }
        if elapsed >= heartbeat {
            eprintln!(
                "admission: waiting (load {:.2}, threshold {:.2}, elapsed {:.1}s, timeout {})",
                load.one,
                threshold,
                elapsed.as_secs_f64(),
                config.timeout
            );
            heartbeat = elapsed + Duration::from_secs(15);
        }
        std::thread::sleep(
            Duration::from_secs(2)
                .min(timeout - elapsed)
                .min(heartbeat - elapsed),
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::{Load, wait_with_samples};
    use crate::config::Admission;
    use std::time::{Duration, Instant};

    #[test]
    fn admission_resamples_after_wait_and_rejects_a_lost_measurement() {
        for readable in [true, false] {
            let mut samples = [
                Some(Load { one: 8.0, cores: 4 }),
                readable.then_some(Load { one: 1.0, cores: 4 }),
            ]
            .into_iter();
            let began = Instant::now();
            let result = wait_with_samples(
                &Admission {
                    max_load: None,
                    timeout: "5s".to_string(),
                },
                || {
                    samples
                        .next()
                        .expect("admission must finish after the second sample")
                },
            );
            assert_eq!(result.is_ok(), readable, "{result:?}");
            assert!(began.elapsed() >= Duration::from_secs(2));
            if let Err(error) = result {
                assert!(
                    error
                        .to_string()
                        .contains("cannot read a valid load average")
                );
            }
        }
    }
}
