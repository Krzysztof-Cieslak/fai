//! Migratable Windows fibers with explicit resume/yield ownership and stack unwinding.

use std::{
    any::Any,
    cell::{Cell, UnsafeCell},
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
};
use windows_sys::Win32::System::Threading::{
    ConvertFiberToThread, ConvertThreadToFiberEx, CreateFiberEx, DeleteFiber, SwitchToFiber,
};
use windows_sys::Win32::System::WindowsProgramming::FIBER_FLAG_FLOAT_SWITCH;

struct Root(*mut c_void);

impl Root {
    fn new() -> Self {
        // SAFETY: scheduler workers enter here on their ordinary OS-thread stack.
        // This TLS owner performs conversion once and reverses it on thread exit.
        let fiber = unsafe { ConvertThreadToFiberEx(std::ptr::null(), FIBER_FLAG_FLOAT_SWITCH) };
        assert!(
            !fiber.is_null(),
            "converting scheduler thread to a fiber: {}",
            std::io::Error::last_os_error()
        );
        Self(fiber)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // SAFETY: the root is thread-local and is destroyed while running on the
        // original thread stack, after all resumes on this worker have returned.
        unsafe {
            ConvertFiberToThread();
        }
    }
}

thread_local! {
    static ROOT: Root = Root::new();
    static CURRENT: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
}

#[inline(never)]
fn current_fiber() -> *mut c_void {
    let current = CURRENT.with(Cell::get);
    if current.is_null() { ROOT.with(|root| root.0) } else { current }
}

#[inline(never)]
unsafe fn switch_fiber(fiber: *mut c_void) {
    CURRENT.with(|current| current.set(fiber));
    // SAFETY: callers supply a live, inactive fiber owned by a coroutine or Root,
    // and serialize all access. Tracking the immediate parent supports nested
    // resumes and unwinding a suspended child while another fiber is running.
    unsafe {
        SwitchToFiber(fiber);
    }
}

struct Core<I, Y> {
    parent: *mut c_void,
    input: Option<I>,
    output: Option<Y>,
    forced: bool,
}

/// One suspension endpoint, at a stable address for the entire fiber lifetime.
pub(super) struct Yielder<I, Y> {
    core: UnsafeCell<Core<I, Y>>,
}

impl<I, Y> Yielder<I, Y> {
    /// Transfers one value to the resuming worker and waits for the next input.
    #[inline(never)]
    pub(super) fn suspend(&self, value: Y) -> I {
        // SAFETY: the scheduler serializes resumes with a task mutex. The fiber
        // and its parent run alternately, never concurrently. No mutable reference
        // into Core is retained across SwitchToFiber or across worker migration.
        unsafe {
            (*self.core.get()).output = Some(value);
            switch_fiber((*self.core.get()).parent);
            if (*self.core.get()).forced {
                resume_unwind(Box::new(ForcedUnwind));
            }
            (*self.core.get()).input.take().expect("fiber resumed without input")
        }
    }
}

type Panic = Box<dyn Any + Send>;
type Body<I, Y, R> = Box<dyn FnOnce(&Yielder<I, Y>, I) -> R>;

struct State<I, Y, R> {
    yielder: Yielder<I, Y>,
    body: UnsafeCell<Option<Body<I, Y, R>>>,
    result: UnsafeCell<Option<Result<R, Panic>>>,
}

struct ForcedUnwind;

/// Result of resuming a fiber exactly once.
pub(super) enum CoroutineResult<Y, R> {
    Yield(Y),
    Return(R),
}

/// Stackful coroutine. Moving across workers requires the scheduler's Send proof.
pub(super) struct Coroutine<I: 'static, Y: 'static, R: 'static> {
    state: Box<State<I, Y, R>>,
    fiber: *mut c_void,
    started: bool,
    finished: bool,
}

impl<I: 'static, Y: 'static, R: 'static> Coroutine<I, Y, R> {
    pub(super) fn new(body: impl FnOnce(&Yielder<I, Y>, I) -> R + 'static) -> Self {
        let mut state = Box::new(State {
            yielder: Yielder {
                core: UnsafeCell::new(Core {
                    parent: std::ptr::null_mut(),
                    input: None,
                    output: None,
                    forced: false,
                }),
            },
            body: UnsafeCell::new(Some(Box::new(body) as Body<I, Y, R>)),
            result: UnsafeCell::new(None),
        });
        // SAFETY: Box provides a stable address. State remains alive until the
        // fiber has returned or its stack has been unwound and deleted.
        let fiber = unsafe {
            CreateFiberEx(
                64 * 1024,
                2 * 1024 * 1024,
                FIBER_FLAG_FLOAT_SWITCH,
                Some(entry::<I, Y, R>),
                std::ptr::from_mut(&mut *state).cast(),
            )
        };
        assert!(!fiber.is_null(), "creating scheduler fiber: {}", std::io::Error::last_os_error());
        Self { state, fiber, started: false, finished: false }
    }

    pub(super) fn resume(&mut self, input: I) -> CoroutineResult<Y, R> {
        assert!(!self.finished, "resuming a completed fiber");
        self.started = true;
        // SAFETY: &mut self and the scheduler mutex exclude concurrent resumes.
        // The new parent is read on this worker before switching to the fiber.
        unsafe {
            (*self.state.yielder.core.get()).input = Some(input);
            (*self.state.yielder.core.get()).parent = current_fiber();
            switch_fiber(self.fiber);
            if let Some(result) = (*self.state.result.get()).take() {
                self.finished = true;
                match result {
                    Ok(value) => CoroutineResult::Return(value),
                    Err(error) => resume_unwind(error),
                }
            } else {
                CoroutineResult::Yield(
                    (*self.state.yielder.core.get())
                        .output
                        .take()
                        .expect("fiber did not yield a value"),
                )
            }
        }
    }
}

unsafe extern "system" fn entry<I: 'static, Y: 'static, R: 'static>(pointer: *mut c_void) {
    // SAFETY: CreateFiberEx receives the live boxed state with these exact type
    // parameters. Panic catching prevents unwinding through the Windows callback.
    unsafe {
        let state = &*pointer.cast::<State<I, Y, R>>();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let body = (*state.body.get()).take().expect("fiber body already entered");
            let input =
                (*state.yielder.core.get()).input.take().expect("fiber entered without input");
            body(&state.yielder, input)
        }));
        *state.result.get() = Some(result);
        switch_fiber((*state.yielder.core.get()).parent);
    }
    // A completed callback must never return into Windows, which exits the thread.
    std::process::abort();
}

impl<I: 'static, Y: 'static, R: 'static> Drop for Coroutine<I, Y, R> {
    fn drop(&mut self) {
        // SAFETY: the coroutine is inactive and exclusively owned. A suspended
        // stack is resumed once with a private unwind request, so its Rust locals
        // are destroyed before DeleteFiber releases the stack mapping.
        unsafe {
            if self.started && !self.finished {
                (*self.state.yielder.core.get()).forced = true;
                (*self.state.yielder.core.get()).parent = current_fiber();
                switch_fiber(self.fiber);
                let result = (*self.state.result.get()).take().expect("fiber failed to unwind");
                if let Err(error) = result
                    && !error.is::<ForcedUnwind>()
                {
                    std::process::abort();
                }
            }
            DeleteFiber(self.fiber);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn suspension_delivers_values_and_returns() {
        let mut coroutine = Coroutine::new(|yielder, value: usize| yielder.suspend(value + 1) + 2);
        assert!(matches!(coroutine.resume(10), CoroutineResult::Yield(11)));
        assert!(matches!(coroutine.resume(20), CoroutineResult::Return(22)));
    }

    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_a_suspended_fiber_destroys_its_stack_locals() {
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let mut coroutine = Coroutine::new(move |yielder, ()| {
            let _owner = CountDrop(captured);
            yielder.suspend(());
        });
        assert!(matches!(coroutine.resume(()), CoroutineResult::Yield(())));
        drop(coroutine);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_fiber_can_resume_on_another_worker() {
        struct SendFiber(Coroutine<(), (), usize>);
        // SAFETY: this test fiber holds only owned integers and transfers exclusive
        // ownership between joined threads, matching the scheduler's Send proof.
        unsafe impl Send for SendFiber {}
        impl SendFiber {
            fn finish(mut self) -> usize {
                match self.0.resume(()) {
                    CoroutineResult::Return(value) => value,
                    _ => panic!("unexpected yield"),
                }
            }
        }
        let coroutine = std::thread::spawn(|| {
            let mut coroutine = Coroutine::new(|yielder, ()| {
                yielder.suspend(());
                42
            });
            assert!(matches!(coroutine.resume(()), CoroutineResult::Yield(())));
            SendFiber(coroutine)
        })
        .join()
        .unwrap();
        assert_eq!(std::thread::spawn(move || coroutine.finish()).join().unwrap(), 42);
    }

    #[test]
    fn nested_resumption_returns_to_the_immediate_parent() {
        let mut parent = Coroutine::new(|_: &Yielder<(), ()>, ()| {
            let mut child = Coroutine::new(|yielder, ()| {
                yielder.suspend(7);
                8
            });
            assert!(matches!(child.resume(()), CoroutineResult::Yield(7)));
            assert!(matches!(child.resume(()), CoroutineResult::Return(8)));
            42
        });
        assert!(matches!(parent.resume(()), CoroutineResult::Return(42)));
    }

    #[test]
    fn a_running_parent_can_unwind_a_suspended_child() {
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let mut child = Coroutine::new(move |yielder, ()| {
            let _owner = CountDrop(captured);
            yielder.suspend(());
        });
        assert!(matches!(child.resume(()), CoroutineResult::Yield(())));
        let mut parent = Coroutine::new(move |_: &Yielder<(), ()>, ()| {
            drop(child);
            42
        });
        assert!(matches!(parent.resume(()), CoroutineResult::Return(42)));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
