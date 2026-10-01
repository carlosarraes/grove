use anyhow::Result;
use std::time::Instant;

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
