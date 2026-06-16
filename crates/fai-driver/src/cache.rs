//! The on-disk, content-addressed object cache.
//!
//! [`object_code`](crate::object_code) is the in-memory (salsa) cache unit; this
//! module adds a **persistent** layer around it so a cold process reuses backend
//! output instead of re-running code generation. The cache is keyed by a portable
//! fingerprint of the reference-counted definition (see
//! [`fai_core::fingerprint_def`]) stamped with the target triple, the compiler
//! version, and the code-generation configuration — so an entry is reused only
//! when the produced object would be byte-identical.
//!
//! It deliberately lives **outside** the salsa query (which stays pure): on a
//! disk hit we skip code generation entirely; on a miss we run the query and
//! write the result back. Writes are atomic (temp file + rename), so concurrent
//! builds across processes are safe. The cache is a pure optimization: any I/O
//! error falls back to in-memory code generation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fai_core::fingerprint_def;
use fai_db::{Db, SourceFile};
use fai_rc::{entry_bounds, rc, result_facts};
use fai_resolve::DefId;
use fai_syntax::Symbol;

use crate::backend::{abi_of, arity_of, object_code, symbol_base};

/// The code-generation configuration stamp, mixed into every object's cache key
/// so a change to how code is generated invalidates stale entries (here, the
/// Cranelift optimization level and the code-generation revision). A future
/// selectable level would thread its real value through here so objects built at
/// different levels never collide. The `int-prims-inlined` token marks the shape
/// where the integer/boolean primitives compile to inline machine code, so a
/// cache warmed before that change can never serve a pre-inlining object. The
/// `reg-direct-call` token marks the register-passing calling convention for
/// direct-callable entries, which changes every direct call and direct-callable
/// entry's machine code. The `divrem-inlined` token marks integer division and
/// remainder compiling to inline machine code (with a constant power-of-two
/// strength-reduced to a shift), so a cache warmed before that change can never
/// serve a pre-inlining object. The `early-drop` token marks that a dead value's
/// drop is emitted before its continuation rather than after — a change to the
/// emitted machine code that leaves the reference-counted IR (and so the
/// fingerprint) untouched, so a cache warmed before it must not serve a stale
/// drop-after object. The `poly-cmp-inlined` token marks structural `=`/`compare`
/// on a possibly-immediate operand (a type variable, or a nullary-bearing
/// union/`List`/empty record) compiling to an inline immediate fast path over the
/// structural runtime fallback, so a cache warmed before that change can never
/// serve a pre-inlining object. The `array-access-inlined` token marks `Array`
/// length/get/set/push compiling to inline loads/stores (with an inline bounds
/// check) rather than runtime calls, so a cache warmed before that change can never
/// serve a pre-inlining object. The `hash-inlined` token marks the structural
/// `hash` of an immediate/`Int`/`Float` operand compiling to an inline splitmix64
/// finalizer over the immediate fast path (with the structural runtime call kept
/// only for boxed operands), so a cache warmed before that change can never serve a
/// pre-inlining object. The `bounds-check-elim` token marks an inline `Array`
/// access whose index a difference-bound analysis proves in range compiling
/// without its bounds check, so a cache warmed before that change can never serve a
/// pre-elision object; the `result-bounds` suffix marks the relational extension
/// (callee result facts and coinductive length preservation threading a bound
/// through a recursive sort, plus the literal-constant and two-variable-subtraction
/// guards), which elides further accesses for the same reference-counted IR. The
/// `array-float-unboxed` token marks an `Array Float`
/// storing its elements as raw, inline `f64`s (self-tagged at runtime) rather than
/// pointers to boxed floats — a representation change to the emitted loads/stores
/// that may leave the reference-counted IR (and so the fingerprint) untouched, so a
/// cache warmed before it must not serve a stale boxed-element object. The
/// `spread-aggregate` token marks a fixed-shape float aggregate held in registers
/// and returned multi-value (scalar replacement of aggregates), which changes a
/// spread-ABI entry's and its callers' machine code, so a cache warmed before that
/// change can never serve a pre-SROA object. The `array-tag-hoisted` token marks a
/// generic array's element-access self-tag computed once per array value (at the
/// function entry or a tail-loop header) and the re-box arm laid out of line —
/// emitted machine code that leaves the reference-counted IR (and so the
/// fingerprint) untouched, so a cache warmed before it must not serve a stale
/// per-access-self-tag object. The `reuse-lambda-export` token marks that a
/// definition with a token-taking reuse entry now exports its lifted-lambda
/// function symbols (so the separate reuse object can link to a capturing lambda it
/// reconstructs), changing the primary object's symbol linkage.
const CODEGEN_CONFIG: &str = "opt=speed;int-prims-inlined;reg-direct-call;divrem-inlined;scalar-float-fields;early-drop;poly-cmp-inlined;array-access-inlined;hash-inlined;bounds-check-elim;result-bounds;array-float-unboxed;spread-aggregate;array-tag-hoisted;reuse-lambda-export";

/// An explicit cache-directory override (set by embedders/tests), taking
/// precedence over `$FAI_CACHE_DIR`. `None` (the default) falls back to the
/// environment and platform defaults.
static CACHE_DIR_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Overrides the cache directory for this process (embedders/tests). `None`
/// restores the default (`$FAI_CACHE_DIR`, then the platform cache dir).
pub fn set_cache_dir(dir: Option<PathBuf>) {
    if let Ok(mut guard) = CACHE_DIR_OVERRIDE.lock() {
        *guard = dir;
    }
}

/// Counts disk-cache hits (object reused from disk), for tests/diagnostics.
static HITS: AtomicU64 = AtomicU64::new(0);
/// Counts disk-cache misses (object generated, then written), for tests.
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Disk-cache hit/miss tallies since process start (or the last [`reset_stats`]).
#[must_use]
pub fn cache_stats() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Resets the disk-cache tallies (tests).
pub fn reset_stats() {
    HITS.store(0, Ordering::Relaxed);
    MISSES.store(0, Ordering::Relaxed);
}

/// The relocatable object for one definition, reusing the on-disk cache.
///
/// On a disk hit the bytes are read back without running code generation; on a
/// miss the in-memory [`object_code`] query produces them and they are written
/// to the cache. With no usable cache directory it falls back to [`object_code`].
pub fn load_or_build_object(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<Vec<u8>> {
    let Some(dir) = cache_dir() else {
        return object_code(db, file, name);
    };
    let def = DefId::new(file.source(db), name);
    let key = object_key(db, file, def);
    let path = object_path(&dir, &key);

    if let Ok(bytes) = std::fs::read(&path) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return Arc::new(bytes);
    }

    let bytes = object_code(db, file, name);
    MISSES.fetch_add(1, Ordering::Relaxed);
    write_atomic(&path, &bytes);
    bytes
}

/// The content key for `def`'s object: a portable fingerprint of its
/// reference-counted IR, stamped with target, compiler version, and config.
fn object_key(db: &dyn Db, file: SourceFile, def: DefId) -> String {
    let lowered = rc(db, file, def.name);
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    let abi = |d: DefId| abi_of(db, d);
    let fingerprint = fingerprint_def(&lowered, &namer, &arity, &abi);

    let mut hasher = blake3::Hasher::new();
    hasher.update(fingerprint.as_bytes());
    hasher.update(b"\0");
    // Bounds-check elimination changes the emitted code from this definition's
    // inferred entry facts and each referenced callee's result facts (both consulted
    // at code generation), so they are part of the key.
    hasher.update(b"bce-entry\0");
    hasher.update(format!("{:?}", entry_bounds(db, file, def.name)).as_bytes());
    hasher.update(b"\0bce-result\0");
    let mut seen = rustc_hash::FxHashSet::default();
    for callee in lowered.referenced_globals() {
        if seen.insert(callee)
            && let Some(cf) = db.source_file(callee.file)
        {
            hasher.update(format!("{:?}", result_facts(db, cf, callee.name)).as_bytes());
            hasher.update(b"\0");
        }
    }
    hasher.update(b"\0");
    hasher.update(target_lexicon::HOST.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(b"\0");
    hasher.update(CODEGEN_CONFIG.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The path of a cached object, sharded by the key's first two hex characters to
/// keep directory sizes bounded.
fn object_path(dir: &Path, key: &str) -> PathBuf {
    let shard = &key[..2.min(key.len())];
    dir.join("objects").join(shard).join(format!("{key}.o"))
}

/// Writes `bytes` to `path` atomically (temp file + rename), creating parents.
/// Errors are ignored: the cache is an optimization, never a correctness input.
fn write_atomic(path: &Path, bytes: &[u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, bytes).is_err() {
        return;
    }
    // A failed rename (e.g. a racing writer already placed the file) is fine:
    // the content is identical, so just drop our temp copy.
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The cache root: an explicit override, else `$FAI_CACHE_DIR`, else the platform
/// user cache dir + `fai`, else `None` (caching disabled).
fn cache_dir() -> Option<PathBuf> {
    if let Ok(guard) = CACHE_DIR_OVERRIDE.lock()
        && let Some(dir) = guard.as_ref()
    {
        return Some(dir.clone());
    }
    if let Some(dir) = std::env::var_os("FAI_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    user_cache_root().map(|root| root.join("fai"))
}

/// The platform user cache directory (no extra dependency).
#[cfg(not(windows))]
fn user_cache_root() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        let path = PathBuf::from(xdg);
        if path.is_absolute() {
            return Some(path);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
}

/// The platform user cache directory on Windows (`%LOCALAPPDATA%`).
#[cfg(windows)]
fn user_cache_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}
