//! Pure compiled regex values and checked native searches. Immutable descriptions
//! define equality; reusable scratch storage is private and bounded per program.

use std::cmp::Ordering;
use std::mem::ManuallyDrop;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

use regex_automata::{
    Input, PatternID,
    meta::{Cache, Regex},
    util::captures::Captures,
};
use regex_syntax::{
    ParserBuilder,
    hir::{Hir, Look},
};

use crate::Value;

static PROGRAMS: AtomicUsize = AtomicUsize::new(0);
static CACHES: AtomicUsize = AtomicUsize::new(0);
static RETAINED: AtomicUsize = AtomicUsize::new(0);

/// Live native program, scratch allocation and retained scratch counts.
#[must_use]
pub fn storage_counts() -> (usize, usize, usize) {
    (
        PROGRAMS.load(AtomicOrdering::Relaxed),
        CACHES.load(AtomicOrdering::Relaxed),
        RETAINED.load(AtomicOrdering::Relaxed),
    )
}

#[derive(Debug)]
struct Error {
    code: i64,
    message: String,
    start: i64,
    end: i64,
}
type Result<T> = std::result::Result<T, Error>;

fn error(code: i64, message: impl Into<String>) -> Error {
    Error { code, message: message.into(), start: -1, end: -1 }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Description {
    pattern: String,
    options: [i64; 7],
}

struct Scratch {
    cache: Cache,
    captures: Captures,
}
impl Drop for Scratch {
    fn drop(&mut self) {
        CACHES.fetch_sub(1, AtomicOrdering::Relaxed);
    }
}

struct Program {
    description: Description,
    regex: Regex,
    scratch: Mutex<Vec<Scratch>>,
}
impl Drop for Program {
    fn drop(&mut self) {
        RETAINED.fetch_sub(
            self.scratch.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).len(),
            AtomicOrdering::Relaxed,
        );
        PROGRAMS.fetch_sub(1, AtomicOrdering::Relaxed);
    }
}

struct Lease<'a> {
    program: &'a Program,
    scratch: Option<Scratch>,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let mut pool =
            self.program.scratch.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if pool.len() < self.program.description.options[6] as usize
            && let Some(scratch) = self.scratch.take()
        {
            pool.push(scratch);
            RETAINED.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }
}

impl Program {
    fn lease(&self) -> Lease<'_> {
        let scratch = self.scratch.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop();
        let scratch = match scratch {
            Some(scratch) => {
                RETAINED.fetch_sub(1, AtomicOrdering::Relaxed);
                scratch
            }
            None => {
                let scratch = Scratch {
                    cache: self.regex.create_cache(),
                    captures: self.regex.create_captures(),
                };
                CACHES.fetch_add(1, AtomicOrdering::Relaxed);
                scratch
            }
        };
        Lease { program: self, scratch: Some(scratch) }
    }
    fn binary(&self) -> bool {
        self.description.options[0] & 1 != 0
    }
}

fn compile(pattern: &str, options: [i64; 7]) -> Result<Arc<Program>> {
    let [flags, max_pattern, max_compiled, max_cache, nesting, groups, retained] = options;
    if !(0..=511).contains(&flags)
        || max_pattern < 0
        || max_compiled <= 0
        || max_cache < 0
        || nesting < 0
        || nesting > u32::MAX as i64
        || groups < 0
        || retained < 0
        || [max_pattern, max_compiled, max_cache, groups, retained]
            .iter()
            .any(|v| usize::try_from(*v).is_err())
    {
        return Err(error(1, "invalid regex compilation options"));
    }
    if pattern.len() > max_pattern as usize {
        return Err(error(4, "pattern byte limit exceeded"));
    }
    let binary = flags & 1 != 0;
    let mut parser = ParserBuilder::new()
        .utf8(!binary)
        .unicode(flags & 2 != 0)
        .case_insensitive(flags & 4 != 0)
        .multi_line(flags & 8 != 0)
        .dot_matches_new_line(flags & 16 != 0)
        .crlf(flags & 32 != 0)
        .ignore_whitespace(flags & 64 != 0)
        .swap_greed(flags & 128 != 0)
        .nest_limit(nesting as u32)
        .build();
    let mut hir = parser.parse(pattern).map_err(|e| {
        let (span, code) = match &e {
            regex_syntax::Error::Parse(e) => {
                use regex_syntax::ast::ErrorKind;
                let code = match e.kind() {
                    ErrorKind::UnsupportedBackreference | ErrorKind::UnsupportedLookAround => 3,
                    ErrorKind::NestLimitExceeded(_) => 11,
                    _ => 2,
                };
                (Some(e.span()), code)
            }
            regex_syntax::Error::Translate(e) => (Some(e.span()), 2),
            _ => (None, 2),
        };
        Error {
            code,
            message: e.to_string(),
            start: span.map_or(-1, |s| s.start.offset as i64),
            end: span.map_or(-1, |s| s.end.offset as i64),
        }
    })?;
    if hir.properties().explicit_captures_len() > groups as usize {
        return Err(error(6, "capture group limit exceeded"));
    }
    if flags & 256 != 0 {
        hir = Hir::concat(vec![Hir::look(Look::Start), hir, Hir::look(Look::End)]);
    }
    let config = Regex::config()
        .utf8_empty(!binary)
        .nfa_size_limit(Some(max_compiled as usize))
        .hybrid_cache_capacity(max_cache as usize)
        .dfa_size_limit(Some(max_compiled as usize / 4));
    let regex = Regex::builder()
        .configure(config)
        .build_from_hir(&hir)
        .map_err(|e| error(5, e.to_string()))?;
    if regex.memory_usage() > max_compiled as usize {
        return Err(error(5, "compiled program byte limit exceeded"));
    }
    PROGRAMS.fetch_add(1, AtomicOrdering::Relaxed);
    Ok(Arc::new(Program {
        description: Description { pattern: pattern.to_owned(), options },
        regex,
        scratch: Mutex::new(Vec::new()),
    }))
}

fn program(value: Value) -> Result<Arc<Program>> {
    // SAFETY: boxed values have a descriptor. Only this module constructs the
    // dedicated regex kind, whose single slot is an Arc<Program> owner.
    if !crate::is_boxed(value)
        || unsafe { crate::object_kind(crate::as_obj(value)) } != crate::KIND_REGEX
    {
        return Err(error(8, "invalid compiled regex value"));
    }
    let raw = unsafe { crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: keep the cell's Arc count intact while cloning a temporary owner.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Program) });
    Ok(Arc::clone(&borrowed))
}

/// Release the native owner when a regex cell dies.
pub(crate) fn drop_program(raw: i64) {
    // SAFETY: raw came from Arc::into_raw in program_value, once per cell.
    drop(unsafe { Arc::from_raw(raw as usize as *const Program) });
}

fn program_value(program: Arc<Program>) -> Value {
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_REGEX_DESC));
    // SAFETY: the one-slot cell takes ownership of this Arc.
    unsafe {
        crate::write_i64(p, crate::HANDLE_PTR_OFFSET, Arc::into_raw(program) as usize as i64)
    };
    crate::from_obj(p)
}

/// Compare immutable descriptions, without inspecting mutable search scratch.
pub(crate) fn compare(a: Value, b: Value) -> Ordering {
    program(a).expect("regex kind").description.cmp(&program(b).expect("regex kind").description)
}

/// Hash the same immutable description used by equality.
pub(crate) fn hash(value: Value) -> u64 {
    let program = program(value).expect("regex kind");
    let mut hash = crate::hash_bytes(program.description.pattern.as_bytes());
    for option in program.description.options {
        hash = crate::hash_combine(hash, option as u64);
    }
    crate::mix64(hash)
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: every supplied field is owned and transferred into the data cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

fn result<T>(result: Result<T>, encode: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => data(0, &[encode(value)]),
        Err(e) => data(
            1,
            &[data(
                0,
                &[
                    crate::fai_box_int(e.code),
                    crate::make_string(e.message.as_bytes()),
                    crate::fai_box_int(e.start),
                    crate::fai_box_int(e.end),
                ],
            )],
        ),
    }
}

fn caught<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(error(9, "regex engine internal failure")))
}

fn integers<const N: usize>(value: Value) -> Result<[i64; N]> {
    // SAFETY: typed intrinsic operands are arrays; validate the descriptor and
    // arity before reading their statically Int element slots.
    if !crate::is_boxed(value)
        || unsafe { crate::object_kind(crate::as_obj(value)) } != crate::KIND_ARRAY
        || unsafe { crate::array_len(value) } != N
    {
        return Err(error(1, "invalid regex request arity"));
    }
    Ok(std::array::from_fn(|i| {
        crate::unbox_int(unsafe {
            crate::read_i64(crate::as_obj(value), crate::ARRAY_ELEMS_OFFSET + i * 8)
        })
    }))
}

fn array(values: Vec<Value>) -> Value {
    let p = crate::alloc_array(values.len(), values.len());
    for (i, value) in values.into_iter().enumerate() {
        // SAFETY: each initialized slot takes one owned value.
        unsafe { crate::write_i64(p, crate::ARRAY_ELEMS_OFFSET + i * 8, value) };
    }
    crate::from_obj(p)
}

/// Compile a String and the standard bridge's seven-option wire tuple.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_compile(pattern: Value, options: Value) -> Value {
    let outcome = caught(|| {
        // SAFETY: the intrinsic's pattern argument has type String.
        compile(unsafe { crate::string_str(pattern) }, integers(options)?)
    });
    crate::fai_drop(pattern);
    crate::fai_drop(options);
    result(outcome, program_value)
}

fn request(
    program: &Program,
    hay: &[u8],
    query: Value,
    binary: bool,
) -> Result<(usize, bool, usize)> {
    let [start, captures, base, limit] = integers(query)?;
    if program.binary() != binary {
        return Err(error(8, "text/binary regex mode mismatch"));
    }
    if limit < 0 || hay.len() as u64 > limit as u64 {
        return Err(error(10, "input byte limit exceeded"));
    }
    if start < 0 || base < 0 || base > start || start as u64 > hay.len() as u64 {
        return Err(error(7, "invalid search offset"));
    }
    if !binary {
        // Text entry points borrow a runtime String, whose UTF-8 is already
        // valid. Check boundary bytes directly rather than revalidating the
        // complete input at every immutable-cursor step.
        let boundary = |at: usize| at == hay.len() || hay[at] & 0xc0 != 0x80;
        if !boundary(start as usize) || !boundary(base as usize) {
            return Err(error(7, "search offset splits a Unicode scalar"));
        }
    }
    Ok((start as usize, captures != 0, base as usize))
}

fn search(
    program: &Program,
    hay: &[u8],
    start: usize,
    captures: bool,
    base: usize,
) -> Vec<[i64; 4]> {
    let mut lease = program.lease();
    let scratch = lease.scratch.as_mut().expect("scratch lease");
    let input = Input::new(hay).span(start..hay.len());
    let spans = if captures {
        program.regex.search_captures_with(&mut scratch.cache, &input, &mut scratch.captures);
        if !scratch.captures.is_match() {
            return Vec::new();
        }
        (0..scratch.captures.group_len())
            .map(|i| scratch.captures.get_group(i).map(|s| (s.start, s.end)))
            .collect::<Vec<_>>()
    } else {
        let Some(m) = program.regex.search_with(&mut scratch.cache, &input) else {
            return Vec::new();
        };
        vec![Some((m.start(), m.end()))]
    };
    let mut points: Vec<_> = spans.iter().flatten().flat_map(|&(a, b)| [a, b]).collect();
    points.sort_unstable();
    points.dedup();
    let mut characters = std::collections::BTreeMap::new();
    if !program.binary() {
        let mut offset = base;
        let mut count = 0;
        for &point in &points {
            count += hay[offset..point].iter().filter(|&&b| b & 0xc0 != 0x80).count();
            characters.insert(point, count);
            offset = point;
        }
    }
    spans
        .into_iter()
        .map(|span| match span {
            None => [-1; 4],
            Some((a, b)) => [
                a as i64,
                b as i64,
                characters.get(&a).copied().unwrap_or(a) as i64,
                characters.get(&b).copied().unwrap_or(b) as i64,
            ],
        })
        .collect()
}

fn find(value: Value, hay: Value, query: Value, binary: bool, existence: bool) -> Value {
    let outcome = caught(|| {
        let program = program(value)?;
        // SAFETY: the two typed entry points supply String and Bytes respectively.
        let input =
            unsafe { if binary { crate::bytes_bytes(hay) } else { crate::string_bytes(hay) } };
        let (start, captures, base) = request(&program, input, query, binary)?;
        if existence {
            let mut lease = program.lease();
            let scratch = lease.scratch.as_mut().expect("scratch lease");
            let found = program
                .regex
                .search_half_with(
                    &mut scratch.cache,
                    &Input::new(input).span(start..input.len()).earliest(true),
                )
                .is_some();
            Ok(if found { vec![[0; 4]] } else { Vec::new() })
        } else {
            Ok(search(&program, input, start, captures, base))
        }
    });
    crate::fai_drop(value);
    crate::fai_drop(hay);
    crate::fai_drop(query);
    result(outcome, |spans| {
        if existence {
            crate::from_bool(!spans.is_empty())
        } else {
            array(spans.into_iter().map(|s| data(0, &s.map(|n| crate::fai_box_int(n)))).collect())
        }
    })
}

/// Find a text match, with byte spans and scalar offsets relative to request base.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_find_text(p: Value, h: Value, q: Value) -> Value {
    find(p, h, q, false, false)
}
/// Find a binary match.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_find_bytes(p: Value, h: Value, q: Value) -> Value {
    find(p, h, q, true, false)
}
/// Test text existence without computing capture locations.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_test_text(p: Value, h: Value, q: Value) -> Value {
    find(p, h, q, false, true)
}
/// Test binary existence without computing capture locations.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_test_bytes(p: Value, h: Value, q: Value) -> Value {
    find(p, h, q, true, true)
}

/// Capture names in numeric order, with None for unnamed and whole-match slots.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_names(value: Value) -> Value {
    let outcome = program(value).map(|p| {
        p.regex
            .group_info()
            .pattern_names(PatternID::ZERO)
            .map(|n| n.map(str::to_owned))
            .collect::<Vec<_>>()
    });
    crate::fai_drop(value);
    result(outcome, |names| {
        array(
            names
                .into_iter()
                .map(|name| {
                    name.map_or(crate::imm_int(0), |n| data(1, &[crate::make_string(n.as_bytes())]))
                })
                .collect(),
        )
    })
}

/// Convert a scalar position to a byte offset and report input byte length.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_byte_index(text: Value, index: Value) -> Value {
    let position = crate::unbox_int(index);
    // SAFETY: the intrinsic is String -> Int -> Result (Int * Int) Error.
    let input = unsafe { crate::string_str(text) };
    let outcome = if position < 0 {
        Err(error(7, "negative scalar index"))
    } else {
        input
            .char_indices()
            .map(|(i, _)| i)
            .chain(Some(input.len()))
            .nth(position as usize)
            .map(|i| (i, input.len()))
            .ok_or_else(|| error(7, "scalar index exceeds input"))
    };
    crate::fai_drop(text);
    crate::fai_drop(index);
    result(outcome, |(i, n)| data(0, &[crate::fai_box_int(i as i64), crate::fai_box_int(n as i64)]))
}

/// Copy a checked UTF-8 byte range. Matches themselves retain only input/ranges.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_slice(text: Value, start: Value, end: Value) -> Value {
    let (a, b) = (crate::unbox_int(start), crate::unbox_int(end));
    // SAFETY: input has static String type; str::get checks both UTF-8 boundaries.
    let input = unsafe { crate::string_str(text) };
    let outcome = if a < 0 || b < a {
        Err(error(7, "invalid text span"))
    } else {
        input
            .get(a as usize..b as usize)
            .map(|s| crate::make_string(s.as_bytes()))
            .ok_or_else(|| error(7, "invalid text span"))
    };
    crate::fai_drop(text);
    crate::fai_drop(start);
    crate::fai_drop(end);
    result(outcome, |v| v)
}

/// Escape pattern metacharacters in Unicode text.
#[unsafe(no_mangle)]
pub extern "C" fn fai_regex_escape(text: Value) -> Value {
    // SAFETY: the intrinsic accepts a String.
    let escaped = regex_syntax::escape(unsafe { crate::string_str(text) });
    crate::fai_drop(text);
    crate::make_string(escaped.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());
    const TEXT: [i64; 7] = [2, 65536, 10485760, 2097152, 128, 256, 1];

    #[test]
    fn text_reports_scalar_and_byte_positions() {
        let _lock = SERIAL.lock().unwrap();
        let program = compile("🙂", TEXT).unwrap();
        assert_eq!(search(&program, "Aé🙂B".as_bytes(), 0, false, 0), [[3, 7, 2, 3]]);
    }
    #[test]
    fn binary_matches_non_utf8() {
        let _lock = SERIAL.lock().unwrap();
        let mut options = TEXT;
        options[0] = 1;
        let program = compile(r"\xFF", options).unwrap();
        assert_eq!(search(&program, &[0, 255, 1], 0, false, 0), [[1, 2, 1, 2]]);
    }
    #[test]
    fn whole_input_chooses_a_later_full_alternative() {
        let _lock = SERIAL.lock().unwrap();
        let mut options = TEXT;
        options[0] |= 256;
        let program = compile("a|ab", options).unwrap();
        assert_eq!(search(&program, b"ab", 0, false, 0), [[0, 2, 0, 2]]);
    }
    #[test]
    fn whole_input_handles_trailing_verbose_comments() {
        let _lock = SERIAL.lock().unwrap();
        let mut options = TEXT;
        options[0] |= 256;
        let program = compile("(?x)a # trailing comment", options).unwrap();
        assert_eq!(search(&program, b"a", 0, false, 0), [[0, 1, 0, 1]]);
    }
    #[test]
    fn optional_and_empty_captures_are_distinct() {
        let _lock = SERIAL.lock().unwrap();
        let program = compile("(a)?(b*)", TEXT).unwrap();
        assert_eq!(
            search(&program, b"", 0, true, 0),
            [[0, 0, 0, 0], [-1, -1, -1, -1], [0, 0, 0, 0]]
        );
    }
    #[test]
    fn syntax_errors_retain_pattern_locations() {
        let _lock = SERIAL.lock().unwrap();
        let e = compile("[", TEXT).err().unwrap();
        assert_eq!((e.code, e.start, e.end), (2, 0, 1));
    }
    #[test]
    fn unsupported_lookaround_is_located() {
        let _lock = SERIAL.lock().unwrap();
        let e = compile("(?=a)", TEXT).err().unwrap();
        assert_eq!((e.code, e.start, e.end), (3, 0, 3));
    }
    #[test]
    fn expanded_program_limit_is_checked() {
        let _lock = SERIAL.lock().unwrap();
        let mut options = TEXT;
        options[2] = 1024;
        assert_eq!(compile("a{100000}", options).err().unwrap().code, 5);
    }
    #[test]
    fn structural_operations_ignore_cache_state() {
        let _lock = SERIAL.lock().unwrap();
        let first = compile("a+", TEXT).unwrap();
        let second = compile("a+", TEXT).unwrap();
        let before = storage_counts();
        let _ = search(&first, b"baaaa", 0, true, 0);
        let a = program_value(first);
        let b = program_value(second);
        assert_eq!(crate::unbox_int(crate::fai_equal(crate::fai_dup(a), crate::fai_dup(b))), 1);
        assert_eq!(crate::fai_hash(crate::fai_dup(a)), crate::fai_hash(crate::fai_dup(b)));
        assert_eq!(crate::unbox_int(crate::fai_compare(crate::fai_dup(a), crate::fai_dup(b))), 0);
        crate::fai_drop(a);
        crate::fai_drop(b);
        assert_eq!(storage_counts(), (before.0 - 2, before.1, before.2));
    }
    #[test]
    fn shared_searches_retain_only_the_configured_scratch_count() {
        let _lock = SERIAL.lock().unwrap();
        let before = storage_counts();
        let program = compile("a+", TEXT).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let p = &program;
                scope.spawn(move || {
                    for _ in 0..100 {
                        assert_eq!(search(p, b"a", 0, false, 0), [[0, 1, 0, 1]]);
                    }
                });
            }
        });
        assert_eq!(storage_counts(), (before.0 + 1, before.1 + 1, before.2 + 1));
        drop(program);
        assert_eq!(storage_counts(), before);
    }

    #[test]
    fn invalid_native_requests_are_checked_before_searching() {
        let _lock = SERIAL.lock().unwrap();
        let program = compile(".", TEXT).unwrap();
        let query = array(vec![
            crate::imm_int(1),
            crate::imm_int(0),
            crate::imm_int(0),
            crate::imm_int(100),
        ]);
        assert_eq!(request(&program, "é".as_bytes(), query, false).err().unwrap().code, 7);
        crate::fai_drop(query);
    }

    #[test]
    fn scratch_storage_does_not_accumulate_across_searches() {
        let _lock = SERIAL.lock().unwrap();
        let before = storage_counts();
        let program = compile("(?<word>[a-z]+)", TEXT).unwrap();
        let _ = search(&program, b"123 repeated text", 0, true, 0);
        let footprint = program.scratch.lock().unwrap()[0].cache.memory_usage();
        for _ in 0..1000 {
            let _ = search(&program, b"123 repeated text", 0, true, 0);
        }
        assert_eq!(program.scratch.lock().unwrap()[0].cache.memory_usage(), footprint);
        assert_eq!(storage_counts(), (before.0 + 1, before.1 + 1, before.2 + 1));
        drop(program);
        assert_eq!(storage_counts(), before);
    }

    #[test]
    fn zero_retention_releases_scratch_after_each_search() {
        let _lock = SERIAL.lock().unwrap();
        let before = storage_counts();
        let mut options = TEXT;
        options[6] = 0;
        let program = compile("a", options).unwrap();
        let _ = search(&program, b"a", 0, false, 0);
        assert_eq!(storage_counts(), (before.0 + 1, before.1, before.2));
        drop(program);
        assert_eq!(storage_counts(), before);
    }

    #[test]
    fn scalar_checkpoint_keeps_absolute_byte_coordinates() {
        let _lock = SERIAL.lock().unwrap();
        let program = compile("(.)", TEXT).unwrap();
        assert_eq!(search(&program, "é🙂".as_bytes(), 2, true, 2), [[2, 6, 0, 1], [2, 6, 0, 1]]);
    }
    mod proptests {
        use super::*;
        use proptest::prelude::*;
        proptest! {
            #[test]
            fn literal_search_matches_the_rust_public_engine(pattern in "[a-z]{0,12}", hay in "[a-z ]{0,80}") {
                let _lock = SERIAL.lock().unwrap();
                let p = compile(&pattern, TEXT).unwrap();
                let oracle = regex::Regex::new(&pattern).unwrap();
                let expected = oracle.find(&hay).map(|m| [m.start() as i64, m.end() as i64, m.start() as i64, m.end() as i64]).into_iter().collect::<Vec<_>>();
                prop_assert_eq!(search(&p, hay.as_bytes(), 0, false, 0), expected);
            }
        }
    }
}
