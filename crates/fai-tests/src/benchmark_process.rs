//! Correctness checks shared by delivered-binary runtime and memory benchmarks.

use std::fmt;
use std::process::{Command, Output};

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
        match algorithm.oracle {
            Oracle::Int(f) => Self::Int(f(algorithm.aot_size)),
            Oracle::Float(f) => Self::Float(f(algorithm.aot_size)),
        }
    }

    /// Checks an execution's status and complete printed answer. NaN, infinity,
    /// malformed output, and extra non-whitespace output are never valid answers.
    pub fn verify(self, label: &str, output: &Output) -> Result<(), Failure> {
        let printed = std::str::from_utf8(&output.stdout).ok().map(str::trim);
        let matches = printed.is_some_and(|printed| match self {
            Self::Int(expected) => printed.parse::<i64>() == Ok(expected),
            Self::Float(expected) => printed.parse::<f64>().is_ok_and(|value| {
                value.is_finite()
                    && expected.is_finite()
                    && (value - expected).abs() < 1e-6 * expected.abs().max(1.0)
            }),
        });
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
