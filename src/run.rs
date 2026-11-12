//! Keep run's stdio alive while a signalled child finishes.
use crate::{config::Admission, supervise::SeedSignals};
use anyhow::Result;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

pub struct Signals {
    signals: SeedSignals,
    began: Instant,
}

impl Signals {
    pub fn capture() -> Result<Self> {
        Ok(Self {
            signals: SeedSignals::capture()?,
            began: Instant::now(),
        })
    }

    pub fn admit(&self, config: &Admission) -> Result<()> {
        crate::load::wait_for_admission_cancellable(config, || self.check_waiting())
    }

    pub fn check_waiting(&self) {
        let signal = self.signals.pending();
        if signal != 0 {
            eprintln!("grove: killed while waiting for load admission, nothing started");
            std::process::exit(128 + signal);
        }
    }

    pub fn wait(&self, child: &mut Child) -> Result<ExitStatus> {
        let mut forwarded = None;
        let mut received_after = Duration::ZERO;
        loop {
            let signal = self.signals.pending();
            if forwarded.is_none() && signal != 0 {
                // SAFETY: the child is still owned and unreaped, so its PID cannot be reused.
                unsafe {
                    libc::kill(child.id() as libc::pid_t, signal);
                }
                received_after = self.began.elapsed();
                forwarded = Some((signal, Instant::now()));
            }
            let status = child.try_wait()?;
            if let Some((signal, sent)) = forwarded {
                if status.is_some() || sent.elapsed() >= Duration::from_secs(10) {
                    let exit = status.map_or_else(
                        || "timeout".to_string(),
                        |status| {
                            status
                                .code()
                                .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
                                .to_string()
                        },
                    );
                    let name = match signal {
                        libc::SIGTERM => "SIGTERM",
                        libc::SIGINT => "SIGINT",
                        _ => "SIGHUP",
                    };
                    eprintln!(
                        "grove: got {name} after {}s, forwarded to child (exit {exit})",
                        received_after.as_secs()
                    );
                    std::process::exit(128 + signal);
                }
            } else if let Some(status) = status {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
