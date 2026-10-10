//! Correctness checks shared by delivered-binary runtime and memory benchmarks.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::algorithms::{Algorithm, Oracle};

/// A precomputed reference answer, kept outside measured subprocess execution.
#[derive(Debug, Clone, Copy)]
pub enum ExpectedAnswer {
    /// An exact signed integer result.
    Int(i64),
    /// A floating-point result, compared with the cross-compiler tolerance.
    Float(f64),
}

impl ExpectedAnswer {
    /// Computes the registered oracle at the delivered binary's workload size.
    #[must_use]
    pub fn for_algorithm(algorithm: &Algorithm) -> Self {
        Self::at_size(algorithm, algorithm.aot_size)
    }

    /// Computes a reference answer for a runtime-supplied workload size.
    #[must_use]
    pub fn at_size(algorithm: &Algorithm, size: i64) -> Self {
        match algorithm.oracle {
            Oracle::Int(f) => Self::Int(f(size)),
            Oracle::Float(f) => Self::Float(f(size)),
        }
    }

    /// Checks one worker response, retaining the same numeric rules as a process.
    pub fn verify_line(self, label: &str, printed: &str) -> Result<(), Failure> {
        if self.matches(printed.trim()) {
            Ok(())
        } else {
            Err(Failure(format!("{label}: expected {self:?}, received {printed:?}")))
        }
    }

    fn matches(self, printed: &str) -> bool {
        match self {
            Self::Int(expected) => printed.parse::<i64>() == Ok(expected),
            Self::Float(expected) => printed.parse::<f64>().is_ok_and(|value| {
                value.is_finite()
                    && expected.is_finite()
                    && (value - expected).abs() < 1e-6 * expected.abs().max(1.0)
            }),
        }
    }

    /// Checks an execution's status and complete printed answer. NaN, infinity,
    /// malformed output, and extra non-whitespace output are never valid answers.
    pub fn verify(self, label: &str, output: &Output) -> Result<(), Failure> {
        let printed = std::str::from_utf8(&output.stdout).ok().map(str::trim);
        let matches = printed.is_some_and(|printed| self.matches(printed));
        if output.status.success() && matches {
            Ok(())
        } else {
            Err(Failure::output(&format!("{label}: expected {self:?}"), output))
        }
    }
}

/// An unsuccessful benchmark process or an answer that disagrees with its oracle.
#[derive(Debug)]
pub struct Failure(String);

impl Failure {
    fn output(context: &str, output: &Output) -> Self {
        Self(format!(
            "{context}; exit status: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failure {}

/// Runs one benchmark process, rejecting every unsuccessful exit, including
/// iterations after the untimed answer check. Output capture is identical for
/// all three language implementations.
pub fn spawn_checked(command: &mut Command) -> Result<Output, Failure> {
    let output = command.output().map_err(|error| Failure(format!("{command:?}: {error}")))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Failure::output(&format!("{command:?}"), &output))
    }
}

/// A persistent native worker with bounded request waits and checked teardown.
pub struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<std::io::Result<String>>,
    reader: Option<JoinHandle<()>>,
    finished: bool,
}

impl Worker {
    /// Starts the child, sends its input-window configuration and awaits `ready`.
    pub fn start(mut command: Command, configuration: &str) -> Result<Self, Failure> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| Failure(format!("{command:?}: {error}")))?;
        let input = child.stdin.take();
        let output = child.stdout.take().expect("piped worker output");
        let (sender, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let failed = line.is_err();
                if sender.send(line).is_err() || failed {
                    break;
                }
            }
        });
        let mut worker = Self { child, input, lines, reader: Some(reader), finished: false };
        worker.send(configuration)?;
        let ready = worker.receive()?;
        if ready != "ready" {
            return Err(Failure(format!("worker did not become ready: {ready:?}")));
        }
        Ok(worker)
    }

    fn send(&mut self, line: &str) -> Result<(), Failure> {
        let input = self.input.as_mut().ok_or_else(|| Failure("worker is closed".into()))?;
        writeln!(input, "{line}")
            .and_then(|()| input.flush())
            .map_err(|error| Failure(format!("worker input: {error}")))
    }

    fn receive(&self) -> Result<String, Failure> {
        self.lines
            .recv_timeout(Duration::from_secs(30))
            .map_err(|error| Failure(format!("worker response: {error}")))?
            .map_err(|error| Failure(format!("worker output: {error}")))
    }

    /// Exchanges one complete line. Error replies are failures, never timings.
    pub fn request(&mut self, line: &str) -> Result<String, Failure> {
        self.send(line)?;
        let reply = self.receive()?;
        if reply.starts_with("error:") {
            Err(Failure(format!("worker rejected {line:?}: {reply}")))
        } else {
            Ok(reply)
        }
    }

    /// Sends EOF, joins the child and verifies its exit status.
    pub fn finish(mut self) -> Result<(), Failure> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<(), Failure> {
        if self.finished {
            return Ok(());
        }
        self.input.take();
        let status = self.child.wait_timeout(Duration::from_secs(2));
        let result = match status {
            Ok(Some(status)) if status.success() => Ok(()),
            Ok(Some(status)) => Err(Failure(format!("worker exit: {status}"))),
            other => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                Err(Failure(format!("worker did not exit after EOF: {other:?}")))
            }
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.finished = true;
        result
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
