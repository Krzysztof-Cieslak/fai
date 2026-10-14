//! Bounded pure compression snapshots. Encoder checkpoints use canonical blocks;
//! decoder checkpoints retain the inflater and its fixed-size history window.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use miniz_oxide::deflate::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress};
use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
use serde::Serialize;

use crate::Value;

const BLOCK: usize = 65536;
const WINDOW: usize = 32768;
const CONTINUE: i64 = 0;
const NEED_INPUT: i64 = 1;
const FLUSHED: i64 = 2;
const MEMBER_END: i64 = 3;
const FINISHED: i64 = 4;
const FAILED: i64 = 5;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static STORAGE: AtomicUsize = AtomicUsize::new(0);

/// Live snapshots and their native retained bytes, excluding Fai output buffers.
pub fn storage_counts() -> (usize, usize) {
    (LIVE.load(AtomicOrdering::Relaxed), STORAGE.load(AtomicOrdering::Relaxed))
}

#[derive(Clone, Serialize)]
struct Options {
    format: u8,
    level: u8,
    input: u64,
    output: u64,
    chunk: usize,
    header: u64,
    members: u64,
}

#[derive(Clone, Debug, Serialize)]
struct Error {
    code: i64,
    message: String,
    offset: i64,
    member: i64,
    input: i64,
    output: i64,
}

#[derive(Clone, Serialize)]
struct Encoder {
    block: Vec<u8>,
    pending: Vec<u8>,
    cursor: usize,
    checksum: u32,
    ended: bool,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
enum Phase {
    Header,
    Body,
    Trailer,
    Between,
    Done,
}

#[derive(Clone, Serialize)]
struct Decoder {
    core: Box<DecompressorOxide>,
    history: Vec<u8>,
    cursor: usize,
    available: usize,
    initialized: usize,
    body_done: bool,
    needs_input: bool,
    phase: Phase,
    small: Vec<u8>,
    stage: u8,
    flags: u8,
    extra: usize,
    header_bytes: u64,
    header_crc: u32,
    checksum: u32,
    member_output: u32,
}

impl Decoder {
    fn new(format: u8) -> Self {
        Self {
            core: Box::default(),
            history: vec![0; WINDOW],
            cursor: 0,
            available: 0,
            initialized: 0,
            body_done: false,
            needs_input: true,
            phase: if format == 0 { Phase::Body } else { Phase::Header },
            small: Vec::new(),
            stage: 0,
            flags: 0,
            extra: 0,
            header_bytes: 0,
            header_crc: 0,
            checksum: if format == 1 { 1 } else { 0 },
            member_output: 0,
        }
    }

    fn next_header_field(&mut self) {
        self.small.clear();
        self.stage = if self.flags & 4 != 0 {
            self.flags &= !4;
            1
        } else if self.flags & 8 != 0 {
            self.flags &= !8;
            3
        } else if self.flags & 16 != 0 {
            self.flags &= !16;
            4
        } else if self.flags & 2 != 0 {
            self.flags &= !2;
            5
        } else {
            self.phase = Phase::Body;
            6
        };
    }
}

#[derive(Clone, Serialize)]
enum Codec {
    Encoder(Encoder),
    Decoder(Decoder),
}

#[derive(Clone, Serialize)]
struct State {
    options: Options,
    codec: Codec,
    input: u64,
    output: u64,
    members: u64,
    error: Option<Error>,
    finishing: bool,
}

impl State {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match &self.codec {
                Codec::Encoder(e) => e.block.capacity() + e.pending.capacity(),
                Codec::Decoder(d) => {
                    std::mem::size_of::<DecompressorOxide>()
                        + d.history.capacity()
                        + d.small.capacity()
                }
            }
            + self.error.as_ref().map_or(0, |e| e.message.capacity())
    }

    fn fail(&mut self, code: i64, message: &str) -> i64 {
        self.error = Some(Error {
            code,
            message: message.into(),
            offset: self.input as i64,
            member: self.members as i64,
            input: self.input as i64,
            output: self.output as i64,
        });
        FAILED
    }

    fn new(wire: [i64; 8]) -> Result<Self, Error> {
        let [encode, format, level, input, output, chunk, header, members] = wire;
        if !(0..=1).contains(&encode)
            || !(0..=2).contains(&format)
            || !(0..=9).contains(&level)
            || input < 0
            || output < 0
            || !(1..=1048576).contains(&chunk)
            || header < 0
            || members < 0
        {
            return Err(Error {
                code: 1,
                message: "invalid compression options".into(),
                offset: -1,
                member: -1,
                input: 0,
                output: 0,
            });
        }
        let options = Options {
            format: format as u8,
            level: level as u8,
            input: input as u64,
            output: output as u64,
            chunk: chunk as usize,
            header: header as u64,
            members: members as u64,
        };
        let codec = if encode != 0 {
            let pending = match format {
                1 => vec![0x78, 0x9c],
                2 => vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255],
                _ => Vec::new(),
            };
            Codec::Encoder(Encoder {
                block: Vec::new(),
                pending,
                cursor: 0,
                checksum: if format == 1 { 1 } else { 0 },
                ended: false,
            })
        } else {
            Codec::Decoder(Decoder::new(format as u8))
        };
        Ok(Self { options, codec, input: 0, output: 0, members: 0, error: None, finishing: false })
    }

    fn step(&mut self, input: &[u8], action: i64) -> (usize, Vec<u8>, i64) {
        if self.error.is_some() {
            return (0, Vec::new(), FAILED);
        }
        if self.finishing && (!input.is_empty() || action == 1) {
            let status = self.fail(6, "input follows declared codec EOF");
            return (0, Vec::new(), status);
        }
        if !(0..=2).contains(&action) || (action == 1 && matches!(self.codec, Codec::Decoder(_))) {
            let status = self.fail(1, "invalid codec action");
            return (0, Vec::new(), status);
        }
        self.finishing |= action == 2;
        if matches!(self.codec, Codec::Encoder(_)) {
            self.encode(input, if self.finishing { 2 } else { action })
        } else {
            self.decode(input, self.finishing)
        }
    }

    fn encode(&mut self, input: &[u8], action: i64) -> (usize, Vec<u8>, i64) {
        let mut consumed = 0;
        let mut output = Vec::new();
        loop {
            let Codec::Encoder(e) = &mut self.codec else { unreachable!() };
            if e.cursor < e.pending.len() {
                let n = (e.pending.len() - e.cursor)
                    .min(self.options.chunk - output.len())
                    .min((self.options.output - self.output) as usize);
                output.extend_from_slice(&e.pending[e.cursor..e.cursor + n]);
                e.cursor += n;
                self.output += n as u64;
                if e.cursor < e.pending.len() {
                    if self.output == self.options.output {
                        let status = self.fail(9, "compression output limit exceeded");
                        return (consumed, output, status);
                    }
                    return (consumed, output, CONTINUE);
                }
                e.pending.clear();
                e.cursor = 0;
            }
            if e.ended {
                if consumed < input.len() {
                    let status = self.fail(6, "input follows a finished encoder");
                    return (consumed, output, status);
                }
                self.members = 1;
                return (consumed, output, FINISHED);
            }
            if output.len() == self.options.chunk {
                return (consumed, output, CONTINUE);
            }
            if consumed < input.len() && consumed < BLOCK && e.block.len() < BLOCK {
                let n = (input.len() - consumed)
                    .min(BLOCK - consumed)
                    .min(BLOCK - e.block.len())
                    .min((self.options.input - self.input) as usize);
                if n == 0 {
                    let status = self.fail(8, "compression input limit exceeded");
                    return (consumed, output, status);
                }
                let part = &input[consumed..consumed + n];
                e.checksum = checksum(self.options.format, e.checksum, part);
                e.block.extend_from_slice(part);
                consumed += n;
                self.input += n as u64;
            }
            let final_input = consumed == input.len();
            if e.block.len() == BLOCK || (final_input && action != 0) {
                let finish = final_input && action == 2;
                if action == 1 && e.block.is_empty() {
                    return (consumed, output, FLUSHED);
                }
                match encode_block(&e.block, self.options.level, finish) {
                    Ok(bytes) => e.pending = bytes,
                    Err(()) => {
                        let status = self.fail(13, "compression engine failed");
                        return (consumed, output, status);
                    }
                }
                e.block.clear();
                e.cursor = 0;
                if finish {
                    match self.options.format {
                        1 => e.pending.extend_from_slice(&e.checksum.to_be_bytes()),
                        2 => {
                            e.pending.extend_from_slice(&e.checksum.to_le_bytes());
                            e.pending.extend_from_slice(&(self.input as u32).to_le_bytes());
                        }
                        _ => {}
                    }
                    e.ended = true;
                }
                continue;
            }
            return (consumed, output, if final_input { NEED_INPUT } else { CONTINUE });
        }
    }

    fn decode(&mut self, input: &[u8], eof: bool) -> (usize, Vec<u8>, i64) {
        let mut consumed = 0;
        let mut output = Vec::new();
        loop {
            let Codec::Decoder(d) = &mut self.codec else { unreachable!() };
            if d.available != 0 {
                let n = d
                    .available
                    .min(self.options.chunk - output.len())
                    .min((self.options.output - self.output) as usize);
                let bytes = &d.history[d.cursor..d.cursor + n];
                output.extend_from_slice(bytes);
                d.checksum = checksum(self.options.format, d.checksum, bytes);
                d.member_output = d.member_output.wrapping_add(n as u32);
                d.cursor = (d.cursor + n) % d.history.len();
                d.available -= n;
                self.output += n as u64;
                if d.available != 0 {
                    if self.output == self.options.output {
                        let status = self.fail(9, "decompression output limit exceeded");
                        return (consumed, output, status);
                    }
                    return (consumed, output, CONTINUE);
                }
            }
            if d.phase == Phase::Done {
                return (consumed, output, FINISHED);
            }
            if d.phase == Phase::Between {
                if consumed == input.len() && eof {
                    d.phase = Phase::Done;
                    return (consumed, output, FINISHED);
                }
                if consumed < input.len() {
                    *d = Decoder::new(self.options.format);
                }
            }
            if d.phase == Phase::Body && d.body_done {
                if self.options.format == 0 {
                    d.phase = Phase::Done;
                    self.members += 1;
                    return (consumed, output, FINISHED);
                }
                d.phase = Phase::Trailer;
                d.small.clear();
            }
            if output.len() == self.options.chunk {
                return (consumed, output, CONTINUE);
            }
            if consumed == input.len() && !eof && (d.phase != Phase::Body || d.needs_input) {
                return (consumed, output, NEED_INPUT);
            }
            if consumed == BLOCK {
                return (consumed, output, CONTINUE);
            }
            if d.phase != Phase::Body && consumed == input.len() {
                let status = self.fail(4, "compressed input ended before its trailer or header");
                return (consumed, output, status);
            }
            if consumed < input.len() && self.input == self.options.input {
                let status = self.fail(8, "decompression input limit exceeded");
                return (consumed, output, status);
            }
            match d.phase {
                Phase::Header => {
                    if self.options.format == 2 && self.members >= self.options.members {
                        let status = self.fail(11, "compressed member limit exceeded");
                        return (consumed, output, status);
                    }
                    if self.options.format == 2 && d.header_bytes == self.options.header {
                        let status = self.fail(10, "gzip header limit exceeded");
                        return (consumed, output, status);
                    }
                    let byte = input[consumed];
                    consumed += 1;
                    self.input += 1;
                    d.header_bytes += 1;
                    let problem = header_byte(d, self.options.format, byte, self.members != 0);
                    if let Some((code, message)) = problem {
                        let status = self.fail(code, message);
                        return (consumed, output, status);
                    }
                }
                Phase::Body => {
                    if self.options.format == 2 && self.members >= self.options.members {
                        let status = self.fail(11, "compressed member limit exceeded");
                        return (consumed, output, status);
                    }
                    let n = (input.len() - consumed)
                        .min(BLOCK - consumed)
                        .min((self.options.input - self.input) as usize);
                    let final_part = eof && consumed + n == input.len();
                    let mut flags =
                        if final_part { 0 } else { inflate_flags::TINFL_FLAG_HAS_MORE_INPUT };
                    // Before the first complete window, distances must not read
                    // its uninitialized prefix. Afterwards wrapping is valid.
                    if d.initialized < d.history.len() {
                        flags |= inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
                    }
                    let (status, used, written) = decompress(
                        &mut d.core,
                        &input[consumed..consumed + n],
                        &mut d.history,
                        d.cursor,
                        flags,
                    );
                    consumed += used;
                    self.input += used as u64;
                    d.available = written;
                    d.initialized = (d.initialized + written).min(d.history.len());
                    d.body_done = status == TINFLStatus::Done;
                    d.needs_input = status == TINFLStatus::NeedsMoreInput;
                    if (status as i32) < 0 {
                        let code = if status == TINFLStatus::FailedCannotMakeProgress && final_part
                        {
                            4
                        } else {
                            3
                        };
                        let status = self.fail(
                            code,
                            if code == 4 {
                                "truncated DEFLATE payload"
                            } else {
                                "invalid DEFLATE payload"
                            },
                        );
                        return (consumed, output, status);
                    }
                    if used == 0 && written == 0 && !d.body_done {
                        if final_part {
                            let status = self.fail(4, "truncated DEFLATE payload");
                            return (consumed, output, status);
                        }
                        d.needs_input = true;
                        return (consumed, output, NEED_INPUT);
                    }
                }
                Phase::Trailer => {
                    d.small.push(input[consumed]);
                    consumed += 1;
                    self.input += 1;
                    let expected = if self.options.format == 1 { 4 } else { 8 };
                    if d.small.len() == expected {
                        let bytes: [u8; 4] = d.small[..4].try_into().expect("four trailer bytes");
                        let actual = if self.options.format == 1 {
                            u32::from_be_bytes(bytes)
                        } else {
                            u32::from_le_bytes(bytes)
                        };
                        let size_ok = self.options.format != 2
                            || u32::from_le_bytes(d.small[4..8].try_into().expect("size trailer"))
                                == d.member_output;
                        if actual != d.checksum || !size_ok {
                            let status = self.fail(5, "compressed checksum or length mismatch");
                            return (consumed, output, status);
                        }
                        self.members += 1;
                        d.phase =
                            if self.options.format == 2 { Phase::Between } else { Phase::Done };
                        return (
                            consumed,
                            output,
                            if self.options.format == 2 { MEMBER_END } else { FINISHED },
                        );
                    }
                }
                Phase::Between => {
                    return (consumed, output, NEED_INPUT);
                }
                Phase::Done => unreachable!(),
            }
        }
    }
}

fn checksum(format: u8, previous: u32, bytes: &[u8]) -> u32 {
    match format {
        1 => miniz_oxide::mz_adler32_oxide(previous, bytes),
        2 => {
            let mut h = crc32fast::Hasher::new_with_initial(previous);
            h.update(bytes);
            h.finalize()
        }
        _ => previous,
    }
}

fn header_byte(
    d: &mut Decoder,
    format: u8,
    byte: u8,
    trailing: bool,
) -> Option<(i64, &'static str)> {
    if format == 1 {
        d.small.push(byte);
        if d.small.len() == 2 {
            let cmf = d.small[0];
            let flags = d.small[1];
            if cmf & 15 != 8
                || cmf >> 4 > 7
                || !(u16::from(cmf) * 256 + u16::from(flags)).is_multiple_of(31)
            {
                return Some((2, "invalid zlib header"));
            }
            if flags & 32 != 0 {
                return Some((7, "zlib preset dictionary is not supported"));
            }
            d.history.resize(1 << ((cmf >> 4) + 8), 0);
            d.small.clear();
            d.phase = Phase::Body;
        }
        return None;
    }
    if d.stage != 5 {
        d.header_crc = checksum(2, d.header_crc, &[byte]);
    }
    match d.stage {
        0 => {
            d.small.push(byte);
            if (d.small.len() == 1 && byte != 0x1f) || (d.small.len() == 2 && byte != 0x8b) {
                return Some((if trailing { 6 } else { 2 }, "invalid gzip magic or trailing data"));
            }
            if d.small.len() == 10 {
                if d.small[2] != 8 || d.small[3] & 0xe0 != 0 {
                    return Some((2, "invalid gzip method or flags"));
                }
                d.flags = d.small[3];
                d.next_header_field();
            }
        }
        1 => {
            d.small.push(byte);
            if d.small.len() == 2 {
                d.extra = usize::from(u16::from_le_bytes([d.small[0], d.small[1]]));
                d.small.clear();
                d.stage = 2;
                if d.extra == 0 {
                    d.next_header_field();
                }
            }
        }
        2 => {
            d.extra -= 1;
            if d.extra == 0 {
                d.next_header_field();
            }
        }
        3 | 4 => {
            if byte == 0 {
                d.next_header_field();
            }
        }
        5 => {
            d.small.push(byte);
            if d.small.len() == 2 {
                if u16::from_le_bytes([d.small[0], d.small[1]]) != d.header_crc as u16 {
                    return Some((5, "gzip header checksum mismatch"));
                }
                d.next_header_field();
            }
        }
        _ => unreachable!(),
    }
    None
}

fn encode_block(input: &[u8], level: u8, finish: bool) -> Result<Vec<u8>, ()> {
    let mut compressor = Box::<CompressorOxide>::default();
    compressor.set_format_and_level(miniz_oxide::DataFormat::Raw, level);
    let mut bytes = vec![0; BLOCK * 2 + 1024];
    let (status, consumed, written) = compress(
        &mut compressor,
        input,
        &mut bytes,
        if finish { TDEFLFlush::Finish } else { TDEFLFlush::Full },
    );
    if consumed != input.len()
        || (finish && status != TDEFLStatus::Done)
        || (!finish && status != TDEFLStatus::Okay)
    {
        return Err(());
    }
    bytes.truncate(written);
    Ok(bytes)
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: each field transfers one owned reference to this data cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

fn error_value(e: &Error) -> Value {
    data(
        0,
        &[
            crate::fai_box_int(e.code),
            crate::make_string(e.message.as_bytes()),
            crate::fai_box_int(e.offset),
            crate::fai_box_int(e.member),
            crate::fai_box_int(e.input),
            crate::fai_box_int(e.output),
        ],
    )
}

fn value(state: State) -> Value {
    LIVE.fetch_add(1, AtomicOrdering::Relaxed);
    STORAGE.fetch_add(state.bytes(), AtomicOrdering::Relaxed);
    let state = Box::into_raw(Box::new(state));
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, &raw const crate::FAI_COMPRESSION_DESC);
    // SAFETY: the dedicated cell owns the boxed State pointer until final drop.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, state as usize as i64) };
    crate::from_obj(p)
}

fn pointer(value: Value) -> Option<*mut State> {
    // SAFETY: kind validation precedes accessing the dedicated payload slot.
    unsafe {
        if !crate::is_boxed(value)
            || crate::object_kind(crate::as_obj(value)) != crate::KIND_COMPRESSION
        {
            return None;
        }
        Some(crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) as usize as *mut State)
    }
}

pub(crate) fn drop_state(raw: i64) {
    // SAFETY: the final cell owner transfers its unique Box pointer here once.
    let state = unsafe { Box::from_raw(raw as usize as *mut State) };
    STORAGE.fetch_sub(state.bytes(), AtomicOrdering::Relaxed);
    LIVE.fetch_sub(1, AtomicOrdering::Relaxed);
}

fn description(value: Value) -> Vec<u8> {
    // SAFETY: structural operations retain an immutable owner for their call.
    let state = unsafe { &*pointer(value).expect("compression value") };
    // Only integer, byte, enum and struct fields are serialized. There are no
    // maps, pointers or capacities in this deterministic internal description.
    serde_json::to_vec(state).expect("serializable codec snapshot")
}

pub(crate) fn compare(a: Value, b: Value) -> Ordering {
    description(a).cmp(&description(b))
}
pub(crate) fn hash(value: Value) -> u64 {
    crate::hash_bytes(&description(value))
}

fn integers<const N: usize>(value: Value) -> Option<[i64; N]> {
    // SAFETY: validate the array kind and length before reading typed Int slots.
    unsafe {
        if !crate::is_boxed(value)
            || crate::object_kind(crate::as_obj(value)) != crate::KIND_ARRAY
            || crate::array_len(value) != N
        {
            return None;
        }
        Some(std::array::from_fn(|i| {
            crate::unbox_int(crate::read_i64(
                crate::as_obj(value),
                crate::ARRAY_ELEMS_OFFSET + i * 8,
            ))
        }))
    }
}

/// Create a checked pure codec value using the standard bridge's option tuple.
#[unsafe(no_mangle)]
pub extern "C" fn fai_compression_create(options: Value) -> Value {
    let result = State::new(integers(options).unwrap_or([-1; 8]));
    crate::fai_drop(options);
    match result {
        Ok(state) => data(0, &[value(state)]),
        Err(e) => data(1, &[error_value(&e)]),
    }
}

/// Consume a snapshot, reusing its native storage only when its Fai cell is unique.
#[unsafe(no_mangle)]
pub extern "C" fn fai_compression_step(state: Value, input: Value, request: Value) -> Value {
    let [start, action] = integers(request).unwrap_or([-1; 2]);
    let source = pointer(state).expect("typed compression snapshot");
    // SAFETY: this call owns state. A shared cell is cloned before mutation;
    // the runtime's count load handles cross-task sharing atomically.
    let owned = unsafe {
        if crate::rc_load(crate::as_obj(state)) == 1 { state } else { value((*source).clone()) }
    };
    if owned != state {
        crate::fai_drop(state);
    }
    // SAFETY: owned now denotes an exclusively owned, dedicated State box.
    let target = unsafe { &mut *pointer(owned).expect("owned compression snapshot") };
    let before = target.bytes();
    // SAFETY: the intrinsic input has the statically checked Bytes type.
    let bytes = unsafe { crate::bytes_bytes(input) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if start < 0 || start as u64 > bytes.len() as u64 {
            (0, Vec::new(), target.fail(1, "invalid compression input offset"))
        } else {
            target.step(&bytes[start as usize..], action)
        }
    }));
    let (used, bytes, status) = result.unwrap_or_else(|_| {
        (0, Vec::new(), target.fail(13, "compression engine internal failure"))
    });
    let after = target.bytes();
    if after >= before {
        STORAGE.fetch_add(after - before, AtomicOrdering::Relaxed);
    } else {
        STORAGE.fetch_sub(before - after, AtomicOrdering::Relaxed);
    }
    let error = target.error.as_ref().map_or(crate::imm_int(0), |e| data(1, &[error_value(e)]));
    let progress =
        [used as i64, status, target.input as i64, target.output as i64, target.members as i64];
    let p = crate::alloc_array(progress.len(), progress.len());
    for (i, number) in progress.into_iter().enumerate() {
        // SAFETY: each freshly allocated slot is initialized once.
        unsafe {
            crate::write_i64(p, crate::ARRAY_ELEMS_OFFSET + i * 8, crate::fai_box_int(number))
        };
    }
    crate::fai_drop(input);
    crate::fai_drop(request);
    data(0, &[owned, crate::make_bytes(&bytes), crate::from_obj(p), error])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn state(encode: bool, format: i64, chunk: i64) -> State {
        State::new([i64::from(encode), format, 6, 8_000_000, 8_000_000, chunk, 65536, 1024])
            .unwrap()
    }

    fn run(state: &mut State, input: &[u8], split: usize) -> Result<Vec<u8>, Error> {
        let mut output = Vec::new();
        let mut offset = 0;
        let mut boundary = split.min(input.len());
        for _ in 0..input.len().saturating_mul(8) + 10000 {
            if offset == boundary && boundary < input.len() {
                boundary = (boundary + split).min(input.len());
            }
            let eof = offset == input.len();
            let (used, bytes, status) =
                state.step(&input[offset..boundary], if eof { 2 } else { 0 });
            offset += used;
            output.extend_from_slice(&bytes);
            if status == FAILED {
                return Err(state.error.clone().unwrap());
            }
            if status == FINISHED {
                if offset != input.len() {
                    state.fail(6, "trailing test input");
                    return Err(state.error.clone().unwrap());
                }
                return Ok(output);
            }
        }
        panic!("codec failed to make bounded progress")
    }

    fn round_trip(format: i64, payload: &[u8]) {
        let packed = run(&mut state(true, format, 13), payload, 7).unwrap();
        let decoded = run(&mut state(false, format, 11), &packed, 3).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn gzip_round_trip() {
        round_trip(2, b"hello gzip");
    }
    #[test]
    fn zlib_round_trip() {
        round_trip(1, b"hello zlib");
    }
    #[test]
    fn raw_round_trip() {
        round_trip(0, b"hello raw");
    }
    #[test]
    fn empty_gzip_round_trip() {
        round_trip(2, b"");
    }
    #[test]
    fn empty_zlib_round_trip() {
        round_trip(1, b"");
    }
    #[test]
    fn empty_raw_round_trip() {
        round_trip(0, b"");
    }
    #[test]
    fn binary_and_block_boundaries_round_trip() {
        let payload: Vec<u8> =
            (0..BLOCK * 3 + 71).map(|i| (i.wrapping_mul(29) ^ (i >> 9)) as u8).collect();
        let packed = run(&mut state(true, 2, 65536), &payload, 8191).unwrap();
        assert_eq!(run(&mut state(false, 2, 65536), &packed, 333).unwrap(), payload);
    }
    #[test]
    fn compression_is_independent_of_feed_chunks() {
        let payload = vec![b'x'; BLOCK * 2 + 37];
        assert_eq!(
            run(&mut state(true, 2, 100), &payload, 17).unwrap(),
            run(&mut state(true, 2, 65536), &payload, BLOCK).unwrap()
        );
    }
    #[test]
    fn zlib_rs_reads_our_gzip() {
        let input = vec![b'a'; BLOCK * 3];
        let packed = run(&mut state(true, 2, 4096), &input, 1024).unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(packed.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }
    #[test]
    fn zlib_rs_reads_our_zlib() {
        let input = b"zlib interoperability";
        let packed = run(&mut state(true, 1, 7), input, 3).unwrap();
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(packed.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }
    #[test]
    fn zlib_rs_reads_our_raw_deflate() {
        let input = b"raw interoperability";
        let packed = run(&mut state(true, 0, 7), input, 3).unwrap();
        let mut decoded = Vec::new();
        flate2::read::DeflateDecoder::new(packed.as_slice()).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, input);
    }
    #[test]
    fn reads_zlib_rs_zlib() {
        let input = b"independent zlib encoder";
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(input).unwrap();
        assert_eq!(run(&mut state(false, 1, 7), &encoder.finish().unwrap(), 1).unwrap(), input);
    }
    #[test]
    fn reads_zlib_rs_raw_deflate() {
        let input = b"independent raw encoder";
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(input).unwrap();
        assert_eq!(run(&mut state(false, 0, 7), &encoder.finish().unwrap(), 1).unwrap(), input);
    }
    #[test]
    fn reads_zlib_rs_dynamic_gzip() {
        let input: Vec<u8> = (0..200000).map(|i| (i % 173) as u8).collect();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&input).unwrap();
        assert_eq!(run(&mut state(false, 2, 19), &encoder.finish().unwrap(), 5).unwrap(), input);
    }
    #[test]
    fn concatenated_members() {
        let mut a = run(&mut state(true, 2, 17), b"first", 1).unwrap();
        a.extend(run(&mut state(true, 2, 17), b"second", 1).unwrap());
        assert_eq!(run(&mut state(false, 2, 2), &a, 1).unwrap(), b"firstsecond");
    }
    #[test]
    fn gzip_trailing_bytes_fail() {
        let mut packed = run(&mut state(true, 2, 17), b"a", 1).unwrap();
        packed.push(b'x');
        assert_eq!(run(&mut state(false, 2, 1), &packed, 1).unwrap_err().code, 6);
    }
    #[test]
    fn checksum_failure() {
        let mut packed = run(&mut state(true, 2, 17), b"payload", 1).unwrap();
        let at = packed.len() - 8;
        packed[at] ^= 1;
        assert_eq!(run(&mut state(false, 2, 1), &packed, 1).unwrap_err().code, 5);
    }
    #[test]
    fn truncated_trailer() {
        let mut packed = run(&mut state(true, 2, 17), b"payload", 1).unwrap();
        packed.pop();
        assert_eq!(run(&mut state(false, 2, 1), &packed, 1).unwrap_err().code, 4);
    }
    #[test]
    fn exact_output_limit_succeeds() {
        let packed = run(&mut state(true, 2, 17), b"abc", 1).unwrap();
        let mut decoder = state(false, 2, 1);
        decoder.options.output = 3;
        assert_eq!(run(&mut decoder, &packed, 1).unwrap(), b"abc");
    }
    #[test]
    fn output_limit_is_cumulative_across_members() {
        let mut packed = run(&mut state(true, 2, 17), b"ab", 1).unwrap();
        packed.extend(run(&mut state(true, 2, 17), b"cd", 1).unwrap());
        let mut decoder = state(false, 2, 1);
        decoder.options.output = 3;
        assert_eq!(run(&mut decoder, &packed, 1).unwrap_err().code, 9);
        assert_eq!(decoder.output, 3);
    }
    #[test]
    fn optional_headers_and_header_checksum() {
        let packed = run(&mut state(true, 2, 17), b"abc", 1).unwrap();
        let mut header = vec![31, 139, 8, 30, 0, 0, 0, 0, 0, 255, 2, 0, 42, 43, b'n', 0, b'c', 0];
        let crc = checksum(2, 0, &header) as u16;
        header.extend(crc.to_le_bytes());
        header.extend_from_slice(&packed[10..]);
        assert_eq!(run(&mut state(false, 2, 1), &header, 1).unwrap(), b"abc");
    }
    #[test]
    fn retained_decoder_snapshot_branches() {
        let packed = run(&mut state(true, 2, 17), b"snapshot", 1).unwrap();
        let mut decoder = state(false, 2, 1);
        let (used, _, _) = decoder.step(&packed[..12], 0);
        let key = serde_json::to_vec(&decoder).unwrap();
        let mut a = decoder.clone();
        let mut b = decoder.clone();
        assert_eq!(
            run(&mut a, &packed[used..], 1).unwrap(),
            run(&mut b, &packed[used..], 1).unwrap()
        );
        assert_eq!(serde_json::to_vec(&a).unwrap(), serde_json::to_vec(&b).unwrap());
        assert_eq!(serde_json::to_vec(&decoder).unwrap(), key);
    }
    #[test]
    fn retained_encoder_snapshot_branches_with_distinct_inputs() {
        let mut initial = state(true, 2, 65536);
        let (used, head, _) = initial.step(b"prefix", 0);
        assert_eq!(used, 6);
        let key = serde_json::to_vec(&initial).unwrap();
        let mut left = head.clone();
        left.extend(run(&mut initial.clone(), b"-left", 1).unwrap());
        let mut right = head;
        right.extend(run(&mut initial.clone(), b"-right", 1).unwrap());
        assert_eq!(run(&mut state(false, 2, 1), &left, 1).unwrap(), b"prefix-left");
        assert_eq!(run(&mut state(false, 2, 1), &right, 1).unwrap(), b"prefix-right");
        assert_eq!(serde_json::to_vec(&initial).unwrap(), key);
    }
    #[test]
    fn retained_storage_stays_bounded() {
        let mut encoder = state(true, 2, 65536);
        let block = vec![b'x'; BLOCK];
        for _ in 0..100 {
            let mut offset = 0;
            while offset < block.len() {
                let (n, _, status) = encoder.step(&block[offset..], 0);
                offset += n;
                assert_ne!(status, FAILED);
            }
            assert!(encoder.bytes() < BLOCK * 4);
        }
    }

    #[test]
    fn sync_flush_can_be_followed_by_more_input() {
        let mut encoder = state(true, 2, 3);
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < 3 {
            let (used, bytes, status) = encoder.step(&b"abc"[offset..], 0);
            offset += used;
            output.extend(bytes);
            assert_ne!(status, FAILED);
        }
        loop {
            let (_, bytes, status) = encoder.step(&[], 1);
            output.extend(bytes);
            if status == FLUSHED {
                break;
            }
            assert_eq!(status, CONTINUE);
        }
        output.extend(run(&mut encoder, b"def", 1).unwrap());
        assert_eq!(run(&mut state(false, 2, 1), &output, 1).unwrap(), b"abcdef");
    }

    #[test]
    fn failures_are_sticky() {
        let mut decoder = state(false, 2, 17);
        assert_eq!(decoder.step(b"x", 0).2, FAILED);
        let before = serde_json::to_vec(&decoder).unwrap();
        assert_eq!(decoder.step(b"other", 0), (0, Vec::new(), FAILED));
        assert_eq!(serde_json::to_vec(&decoder).unwrap(), before);
    }

    #[test]
    fn finish_disallows_later_input_even_while_draining() {
        let mut encoder = state(true, 2, 1);
        assert_eq!(encoder.step(&[], 2).2, CONTINUE);
        assert_eq!(encoder.step(b"x", 0).2, FAILED);
        assert_eq!(encoder.error.unwrap().code, 6);
    }

    #[test]
    fn input_limit_rejects_before_excess_is_consumed() {
        let mut encoder = state(true, 0, 17);
        encoder.options.input = 2;
        let (used, _, status) = encoder.step(b"abc", 0);
        assert_eq!((used, status, encoder.input), (2, CONTINUE, 2));
        let (used, _, status) = encoder.step(b"c", 0);
        assert_eq!((used, status, encoder.input), (0, FAILED, 2));
    }

    #[test]
    fn buffered_matches_drain_without_requesting_more_input() {
        let input = vec![b'x'; WINDOW * 3];
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&input).unwrap();
        let packed = encoder.finish().unwrap();
        let mut decoder = state(false, 0, 1024);
        let mut offset = 0;
        let mut output = Vec::new();
        loop {
            let (used, bytes, status) = decoder.step(&packed[offset..], 0);
            offset += used;
            output.extend(bytes);
            if status == FINISHED {
                break;
            }
            assert_ne!(status, NEED_INPUT, "complete raw input must drain buffered matches");
            assert_eq!(status, CONTINUE);
        }
        assert_eq!(output, input);
    }

    #[test]
    fn back_reference_before_start_is_invalid() {
        let error = run(&mut state(false, 0, 1), &[3, 2, 0], 1).unwrap_err();
        assert_eq!(error.code, 3);
    }

    #[test]
    fn zlib_advertised_window_bounds_distances() {
        let prefix: Vec<u8> = (0..512).map(|i| ((i * 37) ^ (i >> 3)) as u8).collect();
        let input = prefix.repeat(8);
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&input).unwrap();
        let mut packed = encoder.finish().unwrap();
        packed[0] = 8;
        packed[1] = 29;
        assert_eq!(run(&mut state(false, 1, 17), &packed, 3).unwrap_err().code, 3);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;
        proptest! {
            #[test]
            fn arbitrary_binary_round_trip(bytes in prop::collection::vec(any::<u8>(), 0..4096), split in 1usize..127, format in 0i64..3, level in 0u8..10) {
                let mut encoder = state(true, format, 257);
                encoder.options.level = level;
                let packed = run(&mut encoder, &bytes, split).unwrap();
                prop_assert_eq!(run(&mut state(false, format, 113), &packed, split).unwrap(), bytes);
            }
            #[test]
            fn arbitrary_decoding_is_bounded(bytes in prop::collection::vec(any::<u8>(), 0..1024)) {
                let mut decoder = state(false, 0, 31);
                decoder.options.output = 8192;
                let _ = run(&mut decoder, &bytes, 7);
                prop_assert!(decoder.output <= 8192);
            }
        }
    }
}
