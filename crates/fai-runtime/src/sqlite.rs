//! SQLite capability: serialized native connections, scoped transaction leases,
//! owned prepared statements, and cancellation-aware blocking-pool operations.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use libsqlite3_sys as ffi;

use crate::Value;
use crate::scheduler::{CancellationProbe, cancellation_probe};

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq)]
struct Error {
    code: i64,
    message: String,
}

fn error(code: i64, message: impl Into<String>) -> Error {
    Error { code, message: message.into() }
}

fn cancelled() -> Error {
    error(i64::from(ffi::SQLITE_INTERRUPT), "operation cancelled")
}

fn trailing_trivia(mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        if bytes[0].is_ascii_whitespace() || bytes[0] == b';' {
            bytes = &bytes[1..];
        } else if bytes.starts_with(b"--") {
            bytes =
                bytes.iter().position(|&b| b == b'\n' || b == b'\r').map_or(&[], |i| &bytes[i..]);
        } else if bytes.starts_with(b"/*") {
            let Some(end) = bytes[2..].windows(2).position(|w| w == b"*/") else { return true };
            bytes = &bytes[end + 4..];
        } else {
            return false;
        }
    }
    true
}

fn db_error(db: *mut ffi::sqlite3) -> Error {
    // SAFETY: callers hold the connection mutex and db is an open SQLite handle.
    unsafe {
        error(
            i64::from(ffi::sqlite3_extended_errcode(db)),
            CStr::from_ptr(ffi::sqlite3_errmsg(db)).to_string_lossy().into_owned(),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Clone, Debug)]
struct Column {
    name: String,
    declared: Option<String>,
}

struct Prepared(*mut ffi::sqlite3_stmt);

impl Drop for Prepared {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns a prepared statement under the DB lock.
        // finalize also releases failed/exhausted statements; prior step errors
        // were already returned to the caller.
        unsafe { ffi::sqlite3_finalize(self.0) };
    }
}

struct Statement {
    prepared: Prepared,
    lease: u64,
    done: bool,
    failed: Option<Error>,
}

struct State {
    db: *mut ffi::sqlite3,
    busy_ms: u64,
    sequence: u64,
    transactions: Vec<u64>,
    statements: BTreeMap<u64, Statement>,
}

// SAFETY: the SQLite connection and every statement are used exclusively under
// Database's mutex. No SQLite-owned pointers are exposed outside that lock.
unsafe impl Send for State {}

impl State {
    fn allocate_id(&mut self) -> Result<u64> {
        self.sequence =
            self.sequence.checked_add(1).ok_or_else(|| error(-2, "session ID exhausted"))?;
        Ok(self.sequence)
    }

    fn check(&self, lease: u64) -> Result<()> {
        if self.db.is_null() {
            return Err(error(-1, "database is closed"));
        }
        if self.transactions.last().copied().unwrap_or(0) == lease {
            return Ok(());
        }
        if lease == 0 || self.transactions.contains(&lease) {
            Err(error(-2, "use the active transaction session"))
        } else {
            Err(error(-1, "transaction session has ended"))
        }
    }

    fn close(&mut self) {
        self.statements.clear();
        self.transactions.clear();
        if !self.db.is_null() {
            // SAFETY: all owned statements are finalized and the DB is exclusively
            // locked (or exclusively borrowed by Drop). Closing rolls back work.
            unsafe { ffi::sqlite3_close(self.db) };
            self.db = ptr::null_mut();
        }
    }

    fn exec_control(&mut self, sql: &str) -> Result<()> {
        let text = CString::new(sql).map_err(|_| error(21, "NUL in SQL"))?;
        // SAFETY: db is live and locked; SQLite borrows the C string for this call.
        let code = unsafe {
            ffi::sqlite3_exec(self.db, text.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
        };
        if code == ffi::SQLITE_OK { Ok(()) } else { Err(db_error(self.db)) }
    }

    fn prepare(&mut self, sql: &str, parameters: &[Cell]) -> Result<Prepared> {
        let text = CString::new(sql).map_err(|_| error(21, "NUL in SQL text"))?;
        let mut cursor = text.as_ptr();
        let mut statement: *mut ffi::sqlite3_stmt = ptr::null_mut();
        // SAFETY: text remains live, tail points within it, and SQLite handles are
        // exclusively locked. Each non-null statement is immediately owned.
        unsafe {
            while statement.is_null() && !CStr::from_ptr(cursor).is_empty() {
                let mut tail = ptr::null();
                let code = ffi::sqlite3_prepare_v2(self.db, cursor, -1, &mut statement, &mut tail);
                if code != ffi::SQLITE_OK {
                    let failure = db_error(self.db);
                    if !statement.is_null() {
                        ffi::sqlite3_finalize(statement);
                    }
                    return Err(failure);
                }
                if tail == cursor || tail.is_null() {
                    break;
                }
                cursor = tail;
            }
            if statement.is_null() {
                return Err(error(21, "expected one SQL statement"));
            }
            let prepared = Prepared(statement);
            if !trailing_trivia(CStr::from_ptr(cursor).to_bytes()) {
                return Err(error(21, "only one SQL statement is allowed"));
            }
            let expected = ffi::sqlite3_bind_parameter_count(statement) as usize;
            if expected != parameters.len() {
                return Err(error(
                    21,
                    format!("expected {expected} parameters, got {}", parameters.len()),
                ));
            }
            for (index, value) in parameters.iter().enumerate() {
                let slot = (index + 1) as c_int;
                let code = match value {
                    Cell::Null => ffi::sqlite3_bind_null(statement, slot),
                    Cell::Integer(value) => ffi::sqlite3_bind_int64(statement, slot, *value),
                    Cell::Real(value) => {
                        if !value.is_finite() {
                            return Err(error(20, format!("parameter {slot}: non-finite real")));
                        }
                        ffi::sqlite3_bind_double(statement, slot, *value)
                    }
                    Cell::Text(value) => {
                        let len = c_int::try_from(value.len())
                            .map_err(|_| error(18, "text parameter is too large"))?;
                        ffi::sqlite3_bind_text(
                            statement,
                            slot,
                            value.as_ptr().cast(),
                            len,
                            ffi::SQLITE_TRANSIENT(),
                        )
                    }
                    Cell::Blob(value) => {
                        let len = c_int::try_from(value.len())
                            .map_err(|_| error(18, "blob parameter is too large"))?;
                        ffi::sqlite3_bind_blob(
                            statement,
                            slot,
                            value.as_ptr().cast(),
                            len,
                            ffi::SQLITE_TRANSIENT(),
                        )
                    }
                };
                if code != ffi::SQLITE_OK {
                    return Err(db_error(self.db));
                }
            }
            Ok(prepared)
        }
    }

    fn finish(&mut self, lease: u64, commit: bool) -> Result<()> {
        if lease == 0 {
            return Err(error(-2, "the root session is not a transaction"));
        }
        if !self.transactions.contains(&lease) && !commit {
            return Ok(());
        }
        self.check(lease)?;
        self.statements.retain(|_, statement| statement.lease != lease);
        let nested = self.transactions.len() > 1;
        let sql = if commit {
            if nested { format!("RELEASE fai_{lease}") } else { "COMMIT".into() }
        } else if nested {
            format!("ROLLBACK TO fai_{lease}; RELEASE fai_{lease}")
        } else {
            "ROLLBACK".into()
        };
        self.exec_control(&sql)?;
        self.transactions.pop();
        Ok(())
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.close();
    }
}

struct Database(Mutex<State>);

struct Operation {
    cancellation: CancellationProbe,
    started: Instant,
    busy_ms: u64,
    control: bool,
}

unsafe extern "C" fn progress(context: *mut c_void) -> c_int {
    // SAFETY: Hooks retains the boxed Operation while this callback is installed.
    let operation = unsafe { &*context.cast::<Operation>() };
    c_int::from(operation.cancellation.is_cancelled())
}

unsafe extern "C" fn busy(context: *mut c_void, _: c_int) -> c_int {
    // SAFETY: Hooks retains the boxed Operation while this callback is installed.
    let operation = unsafe { &*context.cast::<Operation>() };
    if operation.cancellation.is_cancelled()
        || operation.started.elapsed().as_millis() >= u128::from(operation.busy_ms)
    {
        return 0;
    }
    std::thread::sleep(Duration::from_millis(1));
    1
}

unsafe extern "C" fn authorize(
    context: *mut c_void,
    action: c_int,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> c_int {
    // SAFETY: Hooks retains the boxed Operation while this callback is installed.
    let operation = unsafe { &*context.cast::<Operation>() };
    if matches!(action, ffi::SQLITE_ATTACH | ffi::SQLITE_DETACH)
        || (!operation.control && matches!(action, ffi::SQLITE_TRANSACTION | ffi::SQLITE_SAVEPOINT))
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}

struct Hooks {
    db: *mut ffi::sqlite3,
    _operation: Box<Operation>,
}

impl Hooks {
    fn install(
        db: *mut ffi::sqlite3,
        probe: CancellationProbe,
        busy_ms: u64,
        control: bool,
    ) -> Self {
        let mut operation =
            Box::new(Operation { cancellation: probe, started: Instant::now(), busy_ms, control });
        let context = (&mut *operation as *mut Operation).cast();
        // SAFETY: the connection is exclusively locked and context is stable until
        // Drop removes every callback before releasing its allocation.
        unsafe {
            ffi::sqlite3_progress_handler(db, 1000, Some(progress), context);
            ffi::sqlite3_busy_handler(db, Some(busy), context);
            ffi::sqlite3_set_authorizer(db, Some(authorize), context);
        }
        Self { db, _operation: operation }
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        // SAFETY: the lock and open connection still outlive this guard.
        unsafe {
            ffi::sqlite3_progress_handler(self.db, 0, None, ptr::null_mut());
            ffi::sqlite3_busy_handler(self.db, None, ptr::null_mut());
            ffi::sqlite3_set_authorizer(self.db, None, ptr::null_mut());
        }
    }
}

impl Database {
    fn lock(&self, probe: &CancellationProbe) -> Result<MutexGuard<'_, State>> {
        loop {
            if probe.is_cancelled() {
                return Err(cancelled());
            }
            match self.0.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    fn operation<T>(
        &self,
        probe: &CancellationProbe,
        control: bool,
        action: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.lock(probe)?;
        if state.db.is_null() {
            return Err(error(-1, "database is closed"));
        }
        let _hooks = Hooks::install(state.db, probe.clone(), state.busy_ms, control);
        let result = action(&mut state);
        // SAFETY: the connection is locked and open. SQLite can automatically roll
        // back a transaction on errors; invalidate every affected lease/cursor.
        if !state.transactions.is_empty() && unsafe { ffi::sqlite3_get_autocommit(state.db) } != 0 {
            state.statements.retain(|_, statement| statement.lease == 0);
            state.transactions.clear();
        }
        if result.is_err() && probe.is_cancelled() { Err(cancelled()) } else { result }
    }
}

struct Session {
    database: Arc<Database>,
    lease: u64,
    _parent: Option<Arc<Session>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.lease != 0 {
            let mut state = self.database.0.lock().unwrap_or_else(|e| e.into_inner());
            if !state.db.is_null() && state.transactions.last() == Some(&self.lease) {
                // No callbacks are installed outside an operation. Cleanup must run
                // even on a cancelled task and may not mask the original error.
                let _ = state.finish(self.lease, false);
            }
        }
    }
}

struct Cursor {
    owner: Arc<Session>,
    id: u64,
    columns: Vec<Column>,
}

impl Drop for Cursor {
    fn drop(&mut self) {
        self.owner.database.0.lock().unwrap_or_else(|e| e.into_inner()).statements.remove(&self.id);
    }
}

fn open(
    path: &str,
    read_only: bool,
    busy_ms: i64,
    probe: &CancellationProbe,
) -> Result<Arc<Session>> {
    if probe.is_cancelled() {
        return Err(cancelled());
    }
    if path.is_empty() || busy_ms < 0 {
        return Err(error(21, "path must be nonempty and busy timeout nonnegative"));
    }
    let path = CString::new(path).map_err(|_| error(21, "NUL in database path"))?;
    let mut db = ptr::null_mut();
    let flags = ffi::SQLITE_OPEN_FULLMUTEX
        | if read_only {
            ffi::SQLITE_OPEN_READONLY
        } else {
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE
        };
    // SAFETY: SQLite initializes db, borrowing path only for this call.
    let code = unsafe { ffi::sqlite3_open_v2(path.as_ptr(), &mut db, flags, ptr::null()) };
    if code != ffi::SQLITE_OK {
        let failure = if db.is_null() {
            error(i64::from(code), "could not allocate SQLite connection")
        } else {
            db_error(db)
        };
        if !db.is_null() {
            // SAFETY: no statements exist on this failed connection.
            unsafe { ffi::sqlite3_close(db) };
        }
        return Err(failure);
    }
    // SAFETY: db is a newly opened connection not yet shared.
    unsafe { ffi::sqlite3_extended_result_codes(db, 1) };
    let mut state = State {
        db,
        busy_ms: busy_ms as u64,
        sequence: 0,
        transactions: Vec::new(),
        statements: BTreeMap::new(),
    };
    state.exec_control("PRAGMA foreign_keys = ON")?;
    Ok(Arc::new(Session {
        database: Arc::new(Database(Mutex::new(state))),
        lease: 0,
        _parent: None,
    }))
}

fn metadata(statement: *mut ffi::sqlite3_stmt) -> Result<Vec<Column>> {
    // SAFETY: statement is prepared and exclusively protected by its DB mutex.
    unsafe {
        let count = ffi::sqlite3_column_count(statement);
        (0..count)
            .map(|index| {
                let name = ffi::sqlite3_column_name(statement, index);
                if name.is_null() {
                    return Err(error(7, "could not allocate column name"));
                }
                let declared = ffi::sqlite3_column_decltype(statement, index);
                Ok(Column {
                    name: CStr::from_ptr(name)
                        .to_str()
                        .map_err(|_| error(20, "column name is not valid UTF-8"))?
                        .to_owned(),
                    declared: if declared.is_null() {
                        None
                    } else {
                        Some(
                            CStr::from_ptr(declared)
                                .to_str()
                                .map_err(|_| error(20, "declared type is not valid UTF-8"))?
                                .to_owned(),
                        )
                    },
                })
            })
            .collect()
    }
}

fn execute(
    session: &Session,
    sql: &str,
    parameters: &[Cell],
    probe: &CancellationProbe,
) -> Result<i64> {
    session.database.operation(probe, false, |state| {
        state.check(session.lease)?;
        let statement = state.prepare(sql, parameters)?;
        // SAFETY: statement and DB are live under the exclusive lock.
        unsafe {
            if ffi::sqlite3_column_count(statement.0) != 0 {
                return Err(error(21, "use query for a statement returning columns"));
            }
            let before = ffi::sqlite3_total_changes64(state.db);
            if ffi::sqlite3_step(statement.0) != ffi::SQLITE_DONE {
                return Err(db_error(state.db));
            }
            let changed = ffi::sqlite3_total_changes64(state.db) != before;
            Ok(if changed { ffi::sqlite3_changes64(state.db) } else { 0 })
        }
    })
}

fn query(
    session: &Arc<Session>,
    sql: &str,
    parameters: &[Cell],
    probe: &CancellationProbe,
) -> Result<Arc<Cursor>> {
    session.database.operation(probe, false, |state| {
        state.check(session.lease)?;
        let prepared = state.prepare(sql, parameters)?;
        let columns = metadata(prepared.0)?;
        if columns.is_empty() {
            return Err(error(21, "use execute for a statement without columns"));
        }
        let id = state.allocate_id()?;
        state
            .statements
            .insert(id, Statement { prepared, lease: session.lease, done: false, failed: None });
        Ok(Arc::new(Cursor { owner: Arc::clone(session), id, columns }))
    })
}

fn read_cells(statement: *mut ffi::sqlite3_stmt) -> Result<Vec<Cell>> {
    // SAFETY: SQLite owns each column buffer until the next step/finalize. We copy
    // every value while exclusively holding the connection lock.
    unsafe {
        (0..ffi::sqlite3_column_count(statement))
            .map(|index| {
                Ok(match ffi::sqlite3_column_type(statement, index) {
                    ffi::SQLITE_NULL => Cell::Null,
                    ffi::SQLITE_INTEGER => {
                        Cell::Integer(ffi::sqlite3_column_int64(statement, index))
                    }
                    ffi::SQLITE_FLOAT => Cell::Real(ffi::sqlite3_column_double(statement, index)),
                    kind => {
                        let data = if kind == ffi::SQLITE_TEXT {
                            ffi::sqlite3_column_text(statement, index).cast::<u8>()
                        } else {
                            ffi::sqlite3_column_blob(statement, index).cast::<u8>()
                        };
                        let len = ffi::sqlite3_column_bytes(statement, index) as usize;
                        if len != 0 && data.is_null() {
                            return Err(error(7, "could not read column bytes"));
                        }
                        let bytes = if len == 0 {
                            Vec::new()
                        } else {
                            std::slice::from_raw_parts(data, len).to_vec()
                        };
                        if kind == ffi::SQLITE_TEXT {
                            Cell::Text(String::from_utf8(bytes).map_err(|_| {
                                error(20, format!("column {index}: text is not valid UTF-8"))
                            })?)
                        } else {
                            Cell::Blob(bytes)
                        }
                    }
                })
            })
            .collect()
    }
}

fn next(cursor: &Cursor, probe: &CancellationProbe) -> Result<Option<Vec<Cell>>> {
    cursor.owner.database.operation(probe, false, |state| {
        state.check(cursor.owner.lease)?;
        let db = state.db;
        let statement =
            state.statements.get_mut(&cursor.id).ok_or_else(|| error(-1, "cursor is closed"))?;
        if let Some(error) = &statement.failed {
            return Err(error.clone());
        }
        if statement.done {
            return Ok(None);
        }
        // SAFETY: this prepared statement belongs to the locked connection.
        let code = unsafe { ffi::sqlite3_step(statement.prepared.0) };
        let result = match code {
            ffi::SQLITE_ROW => read_cells(statement.prepared.0).map(Some),
            ffi::SQLITE_DONE => {
                statement.done = true;
                Ok(None)
            }
            _ => Err(db_error(db)),
        };
        if let Err(error) = &result {
            statement.failed = Some(error.clone());
        }
        result
    })
}

fn begin(session: &Arc<Session>, mode: i64, probe: &CancellationProbe) -> Result<Arc<Session>> {
    session.database.operation(probe, true, |state| {
        state.check(session.lease)?;
        if !(0..=2).contains(&mode) {
            return Err(error(21, "unknown transaction mode"));
        }
        if !state.statements.is_empty() {
            return Err(error(-2, "close active cursors before beginning a transaction"));
        }
        let lease = state.allocate_id()?;
        let sql = if state.transactions.is_empty() {
            match mode {
                1 => "BEGIN IMMEDIATE".into(),
                2 => "BEGIN EXCLUSIVE".into(),
                _ => "BEGIN DEFERRED".into(),
            }
        } else {
            format!("SAVEPOINT fai_{lease}")
        };
        state.exec_control(&sql)?;
        state.transactions.push(lease);
        Ok(Arc::new(Session {
            database: Arc::clone(&session.database),
            lease,
            _parent: Some(Arc::clone(session)),
        }))
    })
}

fn commit(session: &Session, probe: &CancellationProbe) -> Result<()> {
    session.database.operation(probe, true, |state| state.finish(session.lease, true))
}

fn rollback(session: &Session) -> Result<()> {
    // Cleanup is intentionally independent of task cancellation.
    session.database.0.lock().unwrap_or_else(|e| e.into_inner()).finish(session.lease, false)
}

fn close(session: &Session) -> Result<()> {
    if session.lease != 0 {
        return rollback(session);
    }
    session.database.0.lock().unwrap_or_else(|e| e.into_inner()).close();
    Ok(())
}

enum Resource {
    Session(Arc<Session>),
    Cursor(Arc<Cursor>),
}

fn handle(resource: Resource) -> Value {
    let raw = Arc::into_raw(Arc::new(resource)) as usize as i64;
    let object = crate::alloc_obj(crate::HEADER_SIZE + 8, &raw const crate::FAI_SQLITE_DESC);
    // SAFETY: the allocated handle owns one Arc pointer in its only slot.
    unsafe { crate::write_i64(object, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(object)
}

/// Release the native resource owned by a dead SQLite handle cell.
pub(crate) fn drop_handle(raw: i64) {
    // SAFETY: the dead Fai cell owns the Arc reference created by handle.
    drop(unsafe { Arc::from_raw(raw as usize as *const Resource) });
}

fn resource(value: Value) -> Arc<Resource> {
    // SAFETY: native Session/Cursor values are live SQLite handle cells.
    let raw = unsafe { crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: retain the Fai cell's reference while cloning an off-worker owner.
    let borrowed = ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const Resource) });
    Arc::clone(&borrowed)
}

fn session(value: Value) -> Result<Arc<Session>> {
    match &*resource(value) {
        Resource::Session(session) => Ok(Arc::clone(session)),
        _ => Err(error(21, "expected a database session")),
    }
}

fn cursor(value: Value) -> Result<Arc<Cursor>> {
    match &*resource(value) {
        Resource::Cursor(cursor) => Ok(Arc::clone(cursor)),
        _ => Err(error(21, "expected a cursor")),
    }
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: all fields are owned and transferred to the new data cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

fn array(fields: Vec<Value>) -> Value {
    let object = crate::alloc_array(fields.len(), fields.len());
    // SAFETY: the allocation has one uniform slot per owned field.
    unsafe {
        for (index, value) in fields.into_iter().enumerate() {
            crate::write_i64(object, crate::ARRAY_ELEMS_OFFSET + index * 8, value);
        }
    }
    crate::from_obj(object)
}

fn result(result: Result<Value>) -> Value {
    match result {
        Ok(value) => data(0, &[value]),
        Err(error) => {
            let fields =
                [crate::fai_box_int(error.code), crate::make_string(error.message.as_bytes())];
            let record = data(0, &fields);
            data(1, &[record])
        }
    }
}

fn unit(result: Result<()>) -> Value {
    self::result(result.map(|()| crate::FAI_UNIT))
}

fn cell(value: Cell) -> Value {
    match value {
        Cell::Null => crate::imm_int(0),
        Cell::Integer(value) => data(1, &[crate::fai_box_int(value)]),
        Cell::Real(value) => data(2, &[crate::fai_box_int(value.to_bits() as i64)]),
        Cell::Text(value) => data(3, &[crate::make_string(value.as_bytes())]),
        Cell::Blob(value) => data(4, &[crate::make_bytes(&value)]),
    }
}

fn parameters(value: Value) -> Result<Vec<Cell>> {
    // SAFETY: the capability signature guarantees a live Array of wire values.
    let count = unsafe { crate::array_len(value) };
    (0..count)
        .map(|index| {
            let element = crate::fai_array_get_borrowed(value, crate::imm_int(index as i64));
            let tag = crate::data_tag_of(element);
            let result = if tag == 0 {
                Ok(Cell::Null)
            } else {
                let field = crate::fai_data_field(element, 0);
                let result = match tag {
                    1 => Ok(Cell::Integer(crate::unbox_int(field))),
                    2 => Ok(Cell::Real(f64::from_bits(crate::unbox_int(field) as u64))),
                    // SAFETY: these variants contain the declared String/Bytes fields.
                    3 => Ok(Cell::Text(unsafe { crate::string_str(field) }.to_owned())),
                    4 => Ok(Cell::Blob(unsafe { crate::bytes_bytes(field) }.to_vec())),
                    _ => Err(error(21, "unknown SQL value")),
                };
                crate::fai_drop(field);
                result
            };
            crate::fai_drop(element);
            result
        })
        .collect()
}

fn blocking<T: Send + 'static>(action: impl FnOnce(CancellationProbe) -> T + Send + 'static) -> T {
    let probe = cancellation_probe();
    if crate::scheduler::in_task() {
        crate::scheduler::run_blocking(Box::new(move || action(probe)))
    } else {
        action(probe)
    }
}

/// Open a native database session. Consumes the path, flag, and timeout values.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_open(path: Value, read_only: Value, busy_ms: Value) -> Value {
    // SAFETY: path has the String type declared by the native capability.
    let text = unsafe { crate::string_str(path) }.to_owned();
    let readonly = crate::unbox_int(read_only) != 0;
    let timeout = crate::unbox_int(busy_ms);
    crate::fai_drop(path);
    crate::fai_drop(read_only);
    crate::fai_drop(busy_ms);
    result(
        blocking(move |probe| open(&text, readonly, timeout, &probe))
            .map(|session| handle(Resource::Session(session))),
    )
}

fn statement_call(database: Value, sql: Value, args: Value, command: bool) -> Value {
    let session = session(database);
    // SAFETY: sql has the String type declared by the native capability.
    let text = unsafe { crate::string_str(sql) }.to_owned();
    let parameters = parameters(args);
    crate::fai_drop(database);
    crate::fai_drop(sql);
    crate::fai_drop(args);
    if command {
        result(
            blocking(move |probe| execute(session?.as_ref(), &text, &parameters?, &probe))
                .map(|value| crate::fai_box_int(value)),
        )
    } else {
        result(
            blocking(move |probe| query(&session?, &text, &parameters?, &probe))
                .map(|cursor| handle(Resource::Cursor(cursor))),
        )
    }
}

/// Execute one bound command, returning its directly affected row count.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_execute(database: Value, sql: Value, args: Value) -> Value {
    statement_call(database, sql, args, true)
}

/// Prepare a bound, row-producing statement without buffering its result set.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_query(database: Value, sql: Value, args: Value) -> Value {
    statement_call(database, sql, args, false)
}

/// Return a cursor's immutable column metadata, including empty result sets.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_columns(value: Value) -> Value {
    let columns = cursor(value).map(|cursor| cursor.columns.clone()).unwrap_or_default();
    crate::fai_drop(value);
    array(
        columns
            .into_iter()
            .map(|column| {
                let declared = match column.declared {
                    None => crate::imm_int(0),
                    Some(text) => data(1, &[crate::make_string(text.as_bytes())]),
                };
                data(0, &[declared, crate::make_string(column.name.as_bytes())])
            })
            .collect(),
    )
}

/// Step a cursor once. Returned cells are independent owned snapshots.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_next(value: Value) -> Value {
    let cursor = cursor(value);
    crate::fai_drop(value);
    result(blocking(move |probe| next(cursor?.as_ref(), &probe)).map(|row| match row {
        None => crate::imm_int(0),
        Some(values) => data(1, &[array(values.into_iter().map(cell).collect())]),
    }))
}

/// Finalize a cursor, including on a cancelled task. Repeated calls are harmless.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_finish(value: Value) -> Value {
    let cursor = cursor(value);
    crate::fai_drop(value);
    unit(blocking(move |_| {
        let cursor = cursor?;
        cursor
            .owner
            .database
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .statements
            .remove(&cursor.id);
        Ok(())
    }))
}

/// Begin a pinned transaction, or a savepoint when called on a child session.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_begin(value: Value, mode: Value) -> Value {
    let session = session(value);
    let mode_value = crate::unbox_int(mode);
    crate::fai_drop(value);
    crate::fai_drop(mode);
    result(
        blocking(move |probe| begin(&session?, mode_value, &probe))
            .map(|session| handle(Resource::Session(session))),
    )
}

/// Commit the current transaction lease; cancellation prevents committing.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_commit(value: Value) -> Value {
    let session = session(value);
    crate::fai_drop(value);
    unit(blocking(move |probe| commit(session?.as_ref(), &probe)))
}

/// Roll back the transaction lease even during cancellation cleanup.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_rollback(value: Value) -> Value {
    let session = session(value);
    crate::fai_drop(value);
    unit(blocking(move |_| rollback(session?.as_ref())))
}

/// End a connection scope and invalidate every retained cursor/session alias.
#[unsafe(no_mangle)]
pub extern "C" fn fai_sqlite_close(value: Value) -> Value {
    let session = session(value);
    crate::fai_drop(value);
    unit(blocking(move |_| close(session?.as_ref())))
}

#[cfg(test)]
mod tests;
