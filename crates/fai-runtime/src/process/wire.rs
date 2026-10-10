//! Uniform-value ABI for the standard-library process host.

use super::*;
use crate::Value;
use std::mem::ManuallyDrop;

fn field(value: Value, index: i64) -> Value {
    crate::fai_data_peek(value, crate::imm_int(index))
}
fn text(value: Value) -> String {
    // SAFETY: the standard host signature supplies a live String.
    unsafe { crate::string_str(value) }.to_owned()
}
fn list(mut value: Value) -> Vec<Value> {
    let mut result = Vec::new();
    while value & 1 == 0 {
        result.push(field(value, 0));
        value = field(value, 1);
    }
    result
}
fn optional(value: Value) -> Option<Value> {
    (value & 1 == 0).then(|| field(value, 0))
}
fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: every field is an owned uniform value transferred to the cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}
fn option(value: Option<Value>) -> Value {
    value.map_or(crate::imm_int(0), |v| data(1, &[v]))
}
fn string(value: &str) -> Value {
    crate::make_string(value.as_bytes())
}
fn integer(value: i64) -> Value {
    crate::fai_box_int(value)
}
fn errors(value: &Error) -> Value {
    data(
        0,
        &[integer(value.code), integer(value.kind), string(&value.message), integer(value.stage)],
    )
}
fn result(value: Result<Value, Error>) -> Value {
    match value {
        Ok(v) => data(0, &[v]),
        Err(e) => data(1, &[errors(&e)]),
    }
}
fn values(items: impl IntoIterator<Item = Value>) -> Value {
    let items: Vec<_> = items.into_iter().collect();
    items.into_iter().rev().fold(crate::imm_int(0), |tail, head| data(1, &[head, tail]))
}
fn exit(value: &Exit) -> Value {
    match value {
        Exit::Code(code) => data(0, &[integer(*code)]),
        Exit::Signal(signal, core) => data(1, &[integer(*signal), integer(i64::from(*core))]),
    }
}
fn session(value: Value) -> Arc<Session> {
    // SAFETY: the opaque host Session is a live process resource cell.
    let raw = unsafe { crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: the cell retains this Arc; ManuallyDrop borrows its reference.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Session) });
    Arc::clone(&borrowed)
}
fn handle(value: Arc<Session>) -> Value {
    let raw = Arc::into_raw(value) as usize as i64;
    let object = crate::alloc_obj(crate::HEADER_SIZE + 8, &raw const crate::FAI_PROCESS_DESC);
    // SAFETY: the new resource cell owns exactly one Arc reference.
    unsafe { crate::write_i64(object, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(object)
}
pub(crate) fn drop_handle(raw: i64) {
    // SAFETY: the dead cell transfers its unique Arc reference to Drop.
    drop(unsafe { Arc::from_raw(raw as usize as *const Session) });
}
fn duration(value: Value) -> Result<Option<Duration>, Error> {
    optional(value)
        .map(|v| {
            let n = crate::unbox_int(v);
            if n <= 0 {
                Err(error(1, 0, "timeout must be positive"))
            } else {
                Ok(Duration::from_millis(n as u64))
            }
        })
        .transpose()
}
fn options(value: Value) -> Result<Options, Error> {
    let buffer = crate::unbox_int(field(value, 0));
    let chunk = crate::unbox_int(field(value, 1));
    let grace = crate::unbox_int(field(value, 3));
    let poll = crate::unbox_int(field(value, 4));
    if !(1..=1_073_741_824).contains(&buffer)
        || !(1..=buffer).contains(&chunk)
        || grace < 0
        || !(1..=1000).contains(&poll)
    {
        return Err(error(1, 0, "invalid process buffer, chunk, grace or poll limit"));
    }
    Ok(Options {
        buffer: buffer as usize,
        chunk: chunk as usize,
        drain: duration(field(value, 2))?,
        grace: Duration::from_millis(grace as u64),
        poll: Duration::from_millis(poll as u64),
        timeout: duration(field(value, 5))?,
    })
}

/// Start one command or a connected pipeline in a single supervised scope.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_start(specs: Value, config: Value) -> Value {
    let commands = list(specs)
        .into_iter()
        .map(|v| Spec {
            arguments: list(field(v, 0)).into_iter().map(text).collect(),
            directory: text(field(v, 1)),
            edits: list(field(v, 2))
                .into_iter()
                .map(|e| (text(field(e, 0)), optional(field(e, 1)).map(text)))
                .collect(),
            executable: text(field(v, 3)),
            inherit: crate::unbox_int(field(v, 4)) != 0,
        })
        .collect();
    let config_result = options(config);
    crate::fai_drop(specs);
    crate::fai_drop(config);
    result(config_result.and_then(|options| Session::start(commands, options)).map(handle))
}

/// Consume the next bounded output chunk, parking until output or completion.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_next(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    result(owned.next().map(|v| {
        option(v.map(|v| {
            data(
                0,
                &[
                    crate::make_bytes(&v.bytes),
                    integer(v.stage as i64),
                    integer(i64::from(v.stderr)),
                ],
            )
        }))
    }))
}

/// Write at most the configured chunk size, returning the completed byte count.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_write(value: Value, bytes: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    // SAFETY: bytes stays owned across the wait; Fai allocations never move.
    let output = owned.write(unsafe { crate::bytes_bytes(bytes) });
    crate::fai_drop(bytes);
    result(output.map(|n| integer(n as i64)))
}

/// Close stdin after its pending write, idempotently.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_end_input(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    result(owned.end_input().map(|()| crate::FAI_UNIT))
}

/// Request graceful shutdown without waiting for the supervisor.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_stop(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    owned.stop();
    crate::FAI_UNIT
}

/// Return observed stage exits, preserving missing statuses during execution.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_status(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    let state = lock(&owned.shared.state);
    values(state.exits.iter().map(|v| option(v.as_ref().map(exit))))
}

/// End the scope, join supervision, and return final exits and all failures.
#[unsafe(no_mangle)]
pub extern "C" fn fai_process_close(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    owned.close();
    let state = lock(&owned.shared.state);
    data(
        0,
        &[
            values(state.cleanup.iter().map(errors)),
            values(state.exits.iter().map(|v| option(v.as_ref().map(exit)))),
            option(state.failure.as_ref().map(errors)),
        ],
    )
}
