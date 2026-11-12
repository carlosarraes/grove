//! Keep run's stdio alive while a signalled child finishes.
use crate::{config::Admission, supervise::SeedSignals};
use anyhow::Result;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

pub struct Signals {
    signals: SeedSignals,
    began: Instant,
    foreground: Option<libc::pid_t>,
}

impl Signals {
    pub fn capture() -> Result<Self> {
        Ok(Self {
            signals: SeedSignals::capture()?,
            began: Instant::now(),
            // Only a foreground invocation may take the controlling terminal.
            foreground: unsafe {
                let group = libc::tcgetpgrp(libc::STDIN_FILENO);
                (group >= 0 && group == libc::getpgrp()).then_some(group)
            },
        })
    }

    pub fn spawn(&self, command: &mut Command) -> Result<Child> {
        command.process_group(0);
        if self.foreground.is_some() {
            // SAFETY: the child callback uses only async-signal-safe libc operations.
            unsafe {
                command.pre_exec(|| set_foreground(libc::getpgrp()));
            }
        }
        Ok(command.spawn()?)
    }

    fn restore_terminal(&self) -> Result<()> {
        if let Some(group) = self.foreground {
            set_foreground(group)?;
        }
        Ok(())
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
                    libc::kill(-(child.id() as libc::pid_t), signal);
                }
                received_after = self.began.elapsed();
                forwarded = Some((signal, Instant::now()));
            }
            let status = child.try_wait()?;
            if let Some((signal, sent)) = forwarded {
                if status.is_some() || sent.elapsed() >= Duration::from_secs(4) {
                    // The shell may exit before a descendant that ignored the signal.
                    // Kill the group even when its leader has already finished.
                    unsafe {
                        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
                    }
                    let exit = status.map_or_else(
                        || {
                            let _ = child.wait();
                            "killed after 4s".to_string()
                        },
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
                        libc::SIGHUP => "SIGHUP",
                        _ => unreachable!("only TERM, INT and HUP are captured"),
                    };
                    eprintln!(
                        "grove: got {name} after {}s, forwarded to child (exit {exit})",
                        received_after.as_secs()
                    );
                    self.restore_terminal()?;
                    std::process::exit(128 + signal);
                }
            } else if let Some(status) = status {
                self.restore_terminal()?;
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// A background supervisor must block SIGTTOU while taking the terminal back.
fn set_foreground(group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTTOU);
        if libc::sigprocmask(libc::SIG_BLOCK, &mask, &mut previous) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = libc::tcsetpgrp(libc::STDIN_FILENO, group);
        let error = std::io::Error::last_os_error();
        libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result != 0 {
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let _ = self.restore_terminal();
    }
}
