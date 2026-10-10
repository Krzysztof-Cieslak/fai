//! Scoped native terminal ownership and deterministic Unicode text primitives.

use crate::Value;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind},
    execute, terminal,
};
use std::io::{IsTerminal, Write};
use std::mem::ManuallyDrop;
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Session {
    closed: AtomicBool,
    output: Mutex<()>,
    reader: Mutex<()>,
}

impl Session {
    fn close(&self) -> Result<(), String> {
        let _output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let display = execute!(
            std::io::stdout(),
            event::DisableMouseCapture,
            event::DisableBracketedPaste,
            event::DisableFocusChange,
            cursor::Show,
            crossterm::style::ResetColor,
            terminal::LeaveAlternateScreen
        )
        .map_err(|e| e.to_string());
        let raw = terminal::disable_raw_mode().map_err(|e| e.to_string());
        display.and(raw)
    }
    fn live(&self) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            Err("terminal session is closed".into())
        } else {
            Ok(())
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn open(mouse: bool) -> Result<Arc<Session>, String> {
    static ACTIVE: OnceLock<Mutex<Weak<Session>>> = OnceLock::new();
    let mut active = ACTIVE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = active.upgrade()
        && (!previous.closed.load(Ordering::Acquire) || previous.reader.try_lock().is_err())
    {
        return Err("terminal is already owned or a previous reader is closing".into());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("full-screen terminal requires interactive stdin and stdout".into());
    }
    if terminal::is_raw_mode_enabled().map_err(|e| e.to_string())? {
        return Err("terminal is already in raw mode".into());
    }
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let session = Arc::new(Session {
        closed: AtomicBool::new(false),
        output: Mutex::new(()),
        reader: Mutex::new(()),
    });
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        cursor::Hide,
        event::EnableBracketedPaste,
        event::EnableFocusChange
    )
    .map_err(|e| e.to_string())?;
    if mouse {
        execute!(std::io::stdout(), event::EnableMouseCapture).map_err(|e| e.to_string())?;
    }
    *active = Arc::downgrade(&session);
    Ok(session)
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: the owned fields transfer into a newly allocated data cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}
fn result(value: Result<Value, String>) -> Value {
    match value {
        Ok(value) => data(0, &[value]),
        Err(error) => data(1, &[crate::make_string(error.as_bytes())]),
    }
}
fn session(value: Value) -> Arc<Session> {
    // SAFETY: the capability type guarantees a live native terminal handle cell.
    let pointer = unsafe { crate::read_i64(crate::as_obj(value), crate::HANDLE_PTR_OFFSET) };
    // SAFETY: borrow the cell's Arc without consuming it, then retain our own owner.
    let reference = ManuallyDrop::new(unsafe { Arc::from_raw(pointer as usize as *const Session) });
    Arc::clone(&reference)
}
fn handle(session: Arc<Session>) -> Value {
    let pointer = Arc::into_raw(session) as usize as i64;
    let object = crate::alloc_obj(crate::HEADER_SIZE + 8, &raw const crate::FAI_TERMINAL_DESC);
    // SAFETY: the fresh handle owns the Arc in its only field.
    unsafe { crate::write_i64(object, crate::HANDLE_PTR_OFFSET, pointer) };
    crate::from_obj(object)
}
pub(crate) fn drop_handle(pointer: i64) {
    // SAFETY: the dead handle transfers its owned Arc to Drop.
    drop(unsafe { Arc::from_raw(pointer as usize as *const Session) });
}
fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    if crate::scheduler::in_task() {
        crate::scheduler::run_blocking(Box::new(work))
    } else {
        work()
    }
}

/// Acquire raw mode and the alternate screen, consuming the mouse flag.
#[unsafe(no_mangle)]
pub extern "C" fn fai_terminal_open(mouse: Value) -> Value {
    let enabled = crate::unbox_int(mouse) != 0;
    crate::fai_drop(mouse);
    result(blocking(move || open(enabled)).map(handle))
}
/// Restore terminal state, including when caller cancellation is sticky.
#[unsafe(no_mangle)]
pub extern "C" fn fai_terminal_close(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    result(blocking(move || owned.close()).map(|()| crate::FAI_UNIT))
}
/// Write terminal bytes, serialized with closure and other writes.
#[unsafe(no_mangle)]
pub extern "C" fn fai_terminal_write(value: Value, bytes: Value) -> Value {
    let owned = session(value);
    // SAFETY: the declared argument is Bytes, retained until this copy finishes.
    let output = unsafe { crate::bytes_bytes(bytes) }.to_vec();
    crate::fai_drop(value);
    crate::fai_drop(bytes);
    let probe = crate::scheduler::cancellation_probe();
    result(
        blocking(move || {
            let _lock = owned.output.lock().unwrap_or_else(|e| e.into_inner());
            owned.live()?;
            if probe.is_cancelled() {
                return Err("terminal write cancelled".into());
            }
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&output).and_then(|()| stdout.flush()).map_err(|e| e.to_string())
        })
        .map(|()| crate::FAI_UNIT),
    )
}
/// Query the current terminal columns and rows.
#[unsafe(no_mangle)]
pub extern "C" fn fai_terminal_size(value: Value) -> Value {
    let owned = session(value);
    crate::fai_drop(value);
    result(owned.live().and_then(|()| terminal::size().map_err(|e| e.to_string())).map(
        |(width, height)| {
            data(0, &[crate::fai_box_int(width.into()), crate::fai_box_int(height.into())])
        },
    ))
}

fn modifiers(value: KeyModifiers) -> i64 {
    i64::from(value.contains(KeyModifiers::SHIFT))
        | (i64::from(value.contains(KeyModifiers::CONTROL)) << 1)
        | (i64::from(value.contains(KeyModifiers::ALT)) << 2)
        | (i64::from(value.contains(KeyModifiers::SUPER)) << 3)
        | (i64::from(value.contains(KeyModifiers::HYPER)) << 4)
        | (i64::from(value.contains(KeyModifiers::META)) << 5)
}
fn key_name(code: KeyCode) -> Option<String> {
    Some(match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Escape".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    })
}
fn event_value(event: Event) -> Option<Value> {
    Some(match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Release {
                return None;
            }
            let name = key_name(key.code)?;
            data(
                0,
                &[data(
                    0,
                    &[
                        crate::make_string(name.as_bytes()),
                        crate::fai_box_int(modifiers(key.modifiers)),
                        crate::from_bool(key.kind == KeyEventKind::Repeat),
                    ],
                )],
            )
        }
        Event::Paste(text) => data(1, &[crate::make_string(text.as_bytes())]),
        Event::Resize(width, height) => {
            data(2, &[crate::fai_box_int(width.into()), crate::fai_box_int(height.into())])
        }
        Event::Mouse(mouse) => {
            let (kind, button) = match mouse.kind {
                MouseEventKind::Down(button) => ("Down", Some(button)),
                MouseEventKind::Up(button) => ("Up", Some(button)),
                MouseEventKind::Drag(button) => ("Drag", Some(button)),
                MouseEventKind::Moved => ("Move", None),
                MouseEventKind::ScrollUp => ("ScrollUp", None),
                MouseEventKind::ScrollDown => ("ScrollDown", None),
                MouseEventKind::ScrollLeft => ("ScrollLeft", None),
                MouseEventKind::ScrollRight => ("ScrollRight", None),
            };
            let button = match button {
                Some(event::MouseButton::Left) => 0,
                Some(event::MouseButton::Right) => 1,
                Some(event::MouseButton::Middle) => 2,
                None => -1,
            };
            data(
                3,
                &[data(
                    0,
                    &[
                        crate::fai_box_int(button),
                        crate::fai_box_int(mouse.column.into()),
                        crate::make_string(kind.as_bytes()),
                        crate::fai_box_int(modifiers(mouse.modifiers)),
                        crate::fai_box_int(mouse.row.into()),
                    ],
                )],
            )
        }
        Event::FocusGained => data(4, &[crate::from_bool(true)]),
        Event::FocusLost => data(4, &[crate::from_bool(false)]),
    })
}
/// Poll input on the blocking pool. The timeout bounds cancellation observation.
#[unsafe(no_mangle)]
pub extern "C" fn fai_terminal_poll(value: Value, timeout: Value) -> Value {
    let owned = session(value);
    let milliseconds = crate::unbox_int(timeout);
    crate::fai_drop(value);
    crate::fai_drop(timeout);
    let probe = crate::scheduler::cancellation_probe();
    let polled = blocking(move || {
        let duration = Duration::from_millis(
            u64::try_from(milliseconds).map_err(|_| "negative terminal poll timeout")?,
        );
        Instant::now().checked_add(duration).ok_or("terminal poll timeout exceeds clock range")?;
        let _reader =
            owned.reader.try_lock().map_err(|_| "terminal already has an input reader")?;
        owned.live()?;
        if probe.is_cancelled() {
            return Err("terminal poll cancelled".to_owned());
        }
        let ready = event::poll(duration).map_err(|e| e.to_string())?;
        owned.live()?;
        if probe.is_cancelled() {
            return Err("terminal poll cancelled".into());
        }
        if ready { event::read().map(Some).map_err(|e| e.to_string()) } else { Ok(None) }
    });
    result(polled.map(|event| {
        event.and_then(event_value).map_or(crate::imm_int(0), |event| data(1, &[event]))
    }))
}

/// Split into extended grapheme clusters, consuming a UTF-8 string.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_graphemes(value: Value) -> Value {
    // SAFETY: the primitive accepts an owned String.
    let text = unsafe { crate::string_str(value) };
    let parts: Vec<_> =
        text.graphemes(true).map(|part| crate::make_string(part.as_bytes())).collect();
    let array = crate::alloc_array(parts.len(), parts.len());
    // SAFETY: each fresh array slot receives one owned string.
    unsafe {
        for (index, part) in parts.into_iter().enumerate() {
            crate::write_i64(array, crate::ARRAY_ELEMS_OFFSET + index * 8, part);
        }
    }
    crate::fai_drop(value);
    crate::from_obj(array)
}
/// Measure display width with an explicit East Asian ambiguous-width choice.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_width(wide: Value, value: Value) -> Value {
    let wide_value = crate::unbox_int(wide) != 0;
    // SAFETY: the primitive accepts an owned String.
    let text = unsafe { crate::string_str(value) };
    let width = if wide_value { text.width_cjk() } else { text.width() };
    crate::fai_drop(wide);
    crate::fai_drop(value);
    crate::fai_box_int(width as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirected_terminal_reports_a_normal_error() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "terminal::tests::terminal_worker", "--nocapture"])
            .env("FAI_TERMINAL_TEST", "redirected")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn terminal_worker() {
        let Ok(case) = std::env::var("FAI_TERMINAL_TEST") else { return };
        if case == "redirected" {
            assert!(open(false).is_err());
            return;
        }
        let session = open(false).unwrap();
        assert!(terminal::is_raw_mode_enabled().unwrap());
        assert!(open(false).is_err(), "a second owner must not disturb the first");
        if case == "close" {
            session.close().unwrap();
            session.close().unwrap();
            assert!(session.live().is_err());
            let replacement = open(false).unwrap();
            assert!(replacement.live().is_ok());
            replacement.close().unwrap();
        }
        drop(session);
        assert!(!terminal::is_raw_mode_enabled().unwrap());
    }

    #[cfg(unix)]
    fn assert_pty_restored(case: &str) {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::CommandExt;
        use wait_timeout::ChildExt;
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: both output pointers are valid and unused optional arguments are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned two fresh owned descriptors, each transferred once.
        let (mut master, slave) =
            unsafe { (std::fs::File::from_raw_fd(master), std::fs::File::from_raw_fd(slave)) };
        let mut before = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: slave is a live terminal descriptor and before is writable storage.
        assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), before.as_mut_ptr()) }, 0);
        // SAFETY: successful tcgetattr initialized every termios field.
        let before = unsafe { before.assume_init() };
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "terminal::tests::terminal_worker", "--nocapture"])
            .env("FAI_TERMINAL_TEST", case)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // SAFETY: the child-only hook uses async-signal-safe syscalls and does not
        // touch shared Rust state. stdin is already the cloned slave terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let result = child.wait_timeout(Duration::from_secs(10)).unwrap();
        if result.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let mut after = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: slave remains live and after is writable termios storage.
        assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), after.as_mut_ptr()) }, 0);
        // SAFETY: successful tcgetattr initialized the result.
        let after = unsafe { after.assume_init() };
        // SAFETY: fcntl only changes this owned descriptor's nonblocking flag.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut output = Vec::new();
        let _ = master.read_to_end(&mut output);
        let output = String::from_utf8_lossy(&output);
        assert!(result.is_some_and(|status| status.success()), "{output}");
        assert!(output.contains("\u{1b}[?1049h") && output.contains("\u{1b}[?1049l"), "{output}");
        assert_eq!(
            (after.c_iflag, after.c_oflag, after.c_cflag, after.c_lflag, after.c_cc),
            (before.c_iflag, before.c_oflag, before.c_cflag, before.c_lflag, before.c_cc)
        );
    }

    #[test]
    #[cfg(unix)]
    fn explicit_close_restores_terminal_and_expires_aliases() {
        assert_pty_restored("close");
    }

    #[test]
    #[cfg(unix)]
    fn final_drop_restores_terminal() {
        assert_pty_restored("drop");
    }
    #[test]
    fn key_mapping_retains_unicode_and_control_modifiers() {
        assert_eq!(key_name(KeyCode::Char('界')), Some("界".into()));
        assert_eq!(modifiers(KeyModifiers::CONTROL | KeyModifiers::SHIFT), 3);
    }
    #[test]
    fn release_events_do_not_repeat_actions() {
        assert!(
            event_value(Event::Key(event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Release
            )))
            .is_none()
        );
    }
    #[test]
    fn unicode_clusters_and_width_cover_joiners_and_accents() {
        let text = "a\u{301}👩‍💻界";
        assert_eq!(text.graphemes(true).collect::<Vec<_>>(), ["a\u{301}", "👩‍💻", "界"]);
        assert_eq!(text.width(), 5);
    }
}
