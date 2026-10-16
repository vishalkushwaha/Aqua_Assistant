//! Running a helper program (whisper-cli, kokoro-tts) so that it can never hold a turn forever.
//!
//! * A deadline on every run: past it the program is killed and the caller gets an error that says it timed out.
//! * Its output is read on separate threads while it runs. A program that prints more than the operating system's
//!   pipe buffer (a few kilobytes) used to block on that write until the deadline, because nobody read the pipe
//!   until the program had exited, so any long transcript failed.
//! * Its input is written on a thread too, so a program that does not read stdin cannot block the writer.
//! * At the deadline the program *and the programs it started* are killed. A launcher such as a pip `.exe` hands the
//!   work to a `python.exe` child; killing only the launcher left the child holding the output pipes, and the caller
//!   then waited for it as if there were no deadline.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long to wait for the output pipes to close after the program has ended or been killed.
const PIPE_GRACE: Duration = Duration::from_secs(3);

/// Kill `child` and everything it started.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}

/// Reads a pipe into a shared buffer as the data arrives, so what was read is still there if the reader is abandoned.
fn spawn_reader<R: Read + Send + 'static>(mut pipe: R) -> (Arc<Mutex<Vec<u8>>>, std::thread::JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = buf.clone();
    let handle = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(&chunk[..n]),
            }
        }
    });
    (buf, handle)
}

/// Wait up to `PIPE_GRACE` for the reader to finish, then take what it has (a program that left a child running with
/// the pipe open does not hold the caller).
fn collect(reader: Option<(Arc<Mutex<Vec<u8>>>, std::thread::JoinHandle<()>)>) -> Vec<u8> {
    let Some((buf, handle)) = reader else { return Vec::new() };
    let until = Instant::now() + PIPE_GRACE;
    while !handle.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = buf.lock().unwrap_or_else(PoisonError::into_inner).clone();
    got
}

/// Run `cmd` to completion or until `timeout`. `stdin` (if any) is written to it; stdout and stderr are returned.
pub fn run_with_deadline(mut cmd: Command, stdin: Option<Vec<u8>>, timeout: Duration) -> Result<Output> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("could not start {:?}", cmd.get_program()))?;

    let writer = stdin.and_then(|bytes| {
        child.stdin.take().map(|mut pipe| {
            std::thread::spawn(move || {
                // A program that exits early closes the pipe: that is not an error here.
                let _ = pipe.write_all(&bytes);
            })
        })
    });
    let out_reader = child.stdout.take().map(spawn_reader);
    let err_reader = child.stderr.take().map(spawn_reader);

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() >= timeout => {
                kill_tree(&mut child);
                let _ = child.wait();
                break Err(anyhow::anyhow!("{:?} timed out after {}s", cmd.get_program(), timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                kill_tree(&mut child);
                break Err(anyhow::anyhow!("waiting for {:?} failed: {e}", cmd.get_program()));
            }
        }
    };
    // The pipes close when the program (and what it started) ends or is killed; a straggler is not waited for.
    if let Some(w) = writer {
        if w.is_finished() {
            let _ = w.join();
        }
    }
    let stdout = collect(out_reader);
    let stderr = collect(err_reader);
    status.map(|status| Output { status, stdout, stderr })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn powershell(script: &str) -> Command {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        c
    }

    #[test]
    fn a_program_that_hangs_is_killed_at_the_deadline_and_the_error_says_so() {
        let started = Instant::now();
        let err = run_with_deadline(powershell("Start-Sleep -Seconds 30"), None, Duration::from_millis(1500)).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "it must not wait for the program: {:?}", started.elapsed());
    }

    #[test]
    fn a_launcher_whose_child_keeps_running_is_still_cut_off_at_the_deadline() {
        // cmd starts a PowerShell child that sleeps and holds the pipes; killing only cmd would leave the call waiting.
        let started = Instant::now();
        let mut c = Command::new("cmd");
        c.args(["/C", "powershell -NoProfile -Command Start-Sleep -Seconds 40"]);
        let err = run_with_deadline(c, None, Duration::from_millis(1500)).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(12), "the child must not hold the call: {:?}", started.elapsed());
    }

    #[test]
    fn output_far_larger_than_a_pipe_buffer_is_returned_in_full() {
        // About 400 KB on stdout and 200 KB on stderr: read while the program runs, never deadlocking on a full pipe.
        let out = run_with_deadline(powershell("1..40000 | ForEach-Object { 'xxxxxxxxx' }; 1..20000 | ForEach-Object { [Console]::Error.WriteLine('yyyyyyyyy') }"), None, Duration::from_secs(60)).unwrap();
        assert!(out.status.success());
        assert!(out.stdout.len() > 300_000 && out.stderr.len() > 150_000, "{} / {}", out.stdout.len(), out.stderr.len());
    }

    #[test]
    fn input_is_delivered_and_the_exit_status_is_reported() {
        let out = run_with_deadline(Command::new("sort"), Some(b"pear\r\napple\r\nfig\r\n".to_vec()), Duration::from_secs(20)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).split_whitespace().collect::<Vec<_>>(), vec!["apple", "fig", "pear"]);
        let failed = run_with_deadline(powershell("exit 3"), None, Duration::from_secs(20)).unwrap();
        assert_eq!(failed.status.code(), Some(3));
        assert!(run_with_deadline(Command::new("no_such_program_xyz"), None, Duration::from_secs(5)).is_err());
    }
}
