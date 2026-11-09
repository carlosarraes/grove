use anyhow::Result;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

pub(crate) fn measure<T>(phase: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    let began = Instant::now();
    let result = work();
    eprintln!(
        "timing: {phase}: {} ({:.3}s)",
        if result.is_ok() { "ok" } else { "failed" },
        began.elapsed().as_secs_f64()
    );
    result
}

pub(crate) fn measure_wait<T>(phase: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    measure(phase, || progress(phase, work))
}

pub(crate) fn progress<T>(phase: &str, work: impl FnOnce() -> T) -> T {
    with_heartbeat(
        heartbeat_interval(),
        |elapsed| eprintln!("{}", format_progress(phase, elapsed)),
        work,
    )
}

fn format_progress(phase: &str, elapsed: Duration) -> String {
    format!("{phase}: still waiting ({}s)", elapsed.as_secs())
}

/// `GROVE_HEARTBEAT_SECS` exists for the test suite; 5s otherwise.
pub(crate) fn heartbeat_interval() -> Duration {
    let secs = std::env::var("GROVE_HEARTBEAT_SECS")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .unwrap_or(5.0);
    Duration::from_secs_f64(secs)
}

/// Run `work`, calling `beat` with the elapsed time every `every` until it returns.
/// A long step under load otherwise looks exactly like a hung one.
pub(crate) fn with_heartbeat<T>(
    every: Duration,
    beat: impl Fn(Duration) + Sync,
    work: impl FnOnce() -> T,
) -> T {
    let (done, finished) = channel::<()>();
    let beat = &beat;
    std::thread::scope(|scope| {
        let began = Instant::now();
        scope.spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = finished.recv_timeout(every) {
                beat(began.elapsed());
            }
        });
        let result = work();
        drop(done);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn heartbeat_beats_while_work_runs_and_stops_after() {
        let beats = Mutex::new(Vec::new());
        let out = with_heartbeat(
            Duration::from_millis(20),
            |elapsed| beats.lock().unwrap().push(elapsed),
            || {
                std::thread::sleep(Duration::from_millis(110));
                7
            },
        );
        assert_eq!(out, 7);
        let count = beats.lock().unwrap().len();
        assert!(count >= 2, "{count} beats");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            beats.lock().unwrap().len(),
            count,
            "beat after the work ended"
        );
    }

    #[test]
    fn quick_work_never_beats() {
        let beats = Mutex::new(0);
        with_heartbeat(
            Duration::from_secs(5),
            |_| *beats.lock().unwrap() += 1,
            || (),
        );
        assert_eq!(*beats.lock().unwrap(), 0);
    }

    #[test]
    fn progress_line_names_the_wait_and_elapsed_seconds() {
        assert_eq!(
            format_progress(
                "resource mongo: docker inspect",
                Duration::from_millis(15_900)
            ),
            "resource mongo: docker inspect: still waiting (15s)"
        );
    }
}
