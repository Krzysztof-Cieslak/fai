//! Runtime-input programs and a checked line protocol for native benchmark workers.

use std::io::{self, BufRead, Write};

use crate::algorithms::{Algorithm, Oracle};
use crate::benchmark_process::ExpectedAnswer;

/// Largest configured input window. Its contents are read before sampling.
pub const MAX_INPUTS: usize = 64;
/// Largest accepted batch, bounding accidental worker requests.
pub const MAX_BATCH: usize = 1_048_576;
/// Portable nonnegative size domain shared with OCaml's native integers.
pub const MAX_SIZE: i64 = i32::MAX as i64;
/// Bounded, canonical checksum modulus for full-width integer results.
pub const MODULUS: i64 = 1_000_000_007;

/// Native entry form: one argv-supplied invocation or a persistent stdin worker.
#[derive(Clone, Copy)]
pub enum Entry {
    /// Read the workload size from the sole application argument.
    Once,
    /// Read an input window and serve checked batches until EOF.
    Worker,
}

/// Reuses the unmodified workload definitions with a runtime-input entry point.
#[must_use]
pub fn fai_program(algorithm: &Algorithm, entry: Entry) -> String {
    let prefix = algorithm.source().split_once("public main :").expect("benchmark main").0;
    assert!(!prefix.contains("benchmarkHarness"), "reserved benchmark helper prefix");
    let floating = matches!(algorithm.oracle, Oracle::Float(_));
    let scalar = if floating { "Float" } else { "Int" };
    let render = format!("{scalar}.toString");
    let suffix = match entry {
        Entry::Once => format!(
            "\npublic main : Runtime -> Unit / {{ Console, Env }}\nlet main runtime =\n  match runtime.env.args () with\n  | [argument] ->\n    match Int.fromString argument with\n    | Some size -> runtime.console.writeLine ({render} ({} size))\n    | None -> runtime.console.writeLine \"error: invalid size\"\n  | _ -> runtime.console.writeLine \"error: expected one size\"\n",
            algorithm.entry
        ),
        Entry::Worker => include_str!("benchmark_aot/worker.fai.in")
            .replace("HARNESS_TYPE", scalar)
            .replace("HARNESS_ENTRY", algorithm.entry)
            .replace("HARNESS_RENDER", &render)
            .replace("HARNESS_ZERO", if floating { "0.0" } else { "0" })
            .replace("HARNESS_FLOOR", if floating { "Int.toFloat value" } else { "value" })
            .replace(
                "HARNESS_ADD",
                if floating {
                    "total + value"
                } else {
                    "let remainder = value % 1000000007\n  let positive = if remainder < 0 then remainder + 1000000007 else remainder\n  (total + positive) % 1000000007"
                },
            )
            .replace("HARNESS_MAX_INPUTS", &MAX_INPUTS.to_string())
            .replace("HARNESS_MAX_BATCH", &MAX_BATCH.to_string())
            .replace("HARNESS_MAX_SIZE", &MAX_SIZE.to_string()),
    };
    format!("{prefix}\n{suffix}")
}

fn words(line: &str) -> impl Iterator<Item = &str> {
    line.trim().split(' ').filter(|word| !word.is_empty())
}

fn inputs(line: &str) -> Option<Vec<i64>> {
    let values: Option<Vec<_>> = words(line)
        .map(|word| word.parse::<i64>().ok().filter(|value| (0..=MAX_SIZE).contains(value)))
        .collect();
    values.filter(|values| !values.is_empty() && values.len() <= MAX_INPUTS)
}

#[inline(always)]
fn fold_int(values: &[i64], count: usize, mut function: impl FnMut(i64) -> i64) -> i64 {
    let mut sum = 0;
    for index in 0..count {
        sum = (sum + function(values[index % values.len()]).rem_euclid(MODULUS)) % MODULUS;
    }
    sum
}

#[inline(always)]
fn fold_float(values: &[i64], count: usize, mut function: impl FnMut(i64) -> f64) -> f64 {
    let mut sum = 0.0;
    for index in 0..count {
        sum += function(values[index % values.len()]);
    }
    sum
}

/// Expected checksum over already-evaluated results, outside the measured call.
#[must_use]
pub fn checksum(values: &[ExpectedAnswer], count: usize) -> ExpectedAnswer {
    assert!(!values.is_empty());
    match values[0] {
        ExpectedAnswer::Int(_) => {
            let mut sum = 0;
            for index in 0..count {
                let ExpectedAnswer::Int(value) = values[index % values.len()] else {
                    panic!("mixed benchmark result types");
                };
                sum = (sum + value.rem_euclid(MODULUS)) % MODULUS;
            }
            ExpectedAnswer::Int(sum)
        }
        ExpectedAnswer::Float(_) => {
            let mut sum = 0.0;
            for index in 0..count {
                let ExpectedAnswer::Float(value) = values[index % values.len()] else {
                    panic!("mixed benchmark result types");
                };
                sum += value;
            }
            ExpectedAnswer::Float(sum)
        }
    }
}

fn write_answer(output: &mut impl Write, answer: ExpectedAnswer) -> io::Result<()> {
    match answer {
        ExpectedAnswer::Int(value) => writeln!(output, "{value}")?,
        ExpectedAnswer::Float(value) => writeln!(output, "{value:?}")?,
    }
    output.flush()
}

fn invalid(output: &mut impl Write, message: &'static str) -> io::Result<()> {
    writeln!(output, "error: {message}")?;
    output.flush()?;
    Err(io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// Runs the native worker protocol. The first line configures 1..64 nonnegative
/// sizes; `value i` returns one full result, `run n` checksums n cyclic inputs,
/// and `floor n` checksums the inputs without running the workload. EOF stops it.
/// Integer checksum reduction is canonical even for negative full-width values.
/// Inlining exposes the worker binary's statically selected workload, matching
/// the direct workload reference in the generated Fai and OCaml workers.
#[inline(always)]
pub fn serve(oracle: Oracle, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return invalid(&mut output, "inputs");
    }
    let Some(values) = inputs(&line) else { return invalid(&mut output, "inputs") };
    writeln!(output, "ready")?;
    output.flush()?;
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut request = words(&line);
        let mode = request.next();
        let count = request.next().and_then(|word| word.parse::<usize>().ok());
        let (Some(mode), Some(count)) = (mode, count) else {
            return invalid(&mut output, "request");
        };
        if request.next().is_some() {
            return invalid(&mut output, "request");
        }
        let answer = match (mode, oracle) {
            ("value", _) if count < values.len() => match oracle {
                Oracle::Int(function) => ExpectedAnswer::Int(function(values[count])),
                Oracle::Float(function) => ExpectedAnswer::Float(function(values[count])),
            },
            ("run", Oracle::Int(function)) if count <= MAX_BATCH => {
                ExpectedAnswer::Int(fold_int(&values, count, function))
            }
            ("run", Oracle::Float(function)) if count <= MAX_BATCH => {
                ExpectedAnswer::Float(fold_float(&values, count, function))
            }
            ("floor", Oracle::Int(_)) if count <= MAX_BATCH => {
                ExpectedAnswer::Int(fold_int(&values, count, |value| value))
            }
            ("floor", Oracle::Float(_)) if count <= MAX_BATCH => {
                ExpectedAnswer::Float(fold_float(&values, count, |value| value as f64))
            }
            _ => return invalid(&mut output, "request"),
        };
        write_answer(&mut output, answer)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_requests_cycle_runtime_inputs() {
        let mut output = Vec::new();
        serve(Oracle::Int(|n| n * 2), &b"1 2 3\nrun 5\nfloor 5\nvalue 2\n"[..], &mut output)
            .unwrap();
        assert_eq!(output, b"ready\n18\n9\n6\n");
    }

    #[test]
    fn zero_batch_does_not_invoke_the_workload() {
        assert_eq!(fold_int(&[1], 0, |_| panic!("unexpected invocation")), 0);
    }

    #[test]
    fn negative_full_width_results_use_canonical_checksums() {
        let mut output = Vec::new();
        serve(Oracle::Int(|_| i64::MIN), &b"0\nvalue 0\nrun 2\n"[..], &mut output).unwrap();
        assert_eq!(
            output,
            format!("ready\n{}\n{}\n", i64::MIN, 2 * i64::MIN.rem_euclid(MODULUS) % MODULUS)
                .as_bytes()
        );
    }

    #[test]
    fn floating_results_keep_their_accumulation_order() {
        let mut output = Vec::new();
        serve(
            Oracle::Float(|n| n as f64 + 0.5),
            &b"1 3\nrun 3\nfloor 3\nvalue 1\n"[..],
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"ready\n6.5\n5.0\n3.5\n");
    }

    #[test]
    fn malformed_configuration_is_reported() {
        let mut output = Vec::new();
        let error = serve(Oracle::Int(|n| n), &b"1 nope 2\n"[..], &mut output).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(output, b"error: inputs\n");
    }

    #[test]
    fn oversized_batch_is_reported() {
        let mut output = Vec::new();
        let input = format!("1\nrun {}\n", MAX_BATCH + 1);
        assert!(serve(Oracle::Int(|n| n), input.as_bytes(), &mut output).is_err());
        assert_eq!(output, b"ready\nerror: request\n");
    }

    #[test]
    fn out_of_range_value_index_is_reported() {
        let mut output = Vec::new();
        assert!(serve(Oracle::Int(|n| n), &b"1\nvalue 1\n"[..], &mut output).is_err());
        assert_eq!(output, b"ready\nerror: request\n");
    }

    #[test]
    fn empty_input_window_is_rejected() {
        let mut output = Vec::new();
        assert!(serve(Oracle::Int(|n| n), &b"\n"[..], &mut output).is_err());
        assert_eq!(output, b"error: inputs\n");
    }

    #[test]
    fn oversized_input_window_is_rejected() {
        let mut output = Vec::new();
        let input = "1 ".repeat(MAX_INPUTS + 1) + "\n";
        assert!(serve(Oracle::Int(|n| n), input.as_bytes(), &mut output).is_err());
        assert_eq!(output, b"error: inputs\n");
    }

    #[test]
    fn extra_request_tokens_are_rejected() {
        let mut output = Vec::new();
        assert!(serve(Oracle::Int(|n| n), &b"1\nrun 1 extra\n"[..], &mut output).is_err());
        assert_eq!(output, b"ready\nerror: request\n");
    }
}
