//! The `wintrap` crate allows a Windows process to trap one or more abstracted
//! "signals", running a callback function in a dedicated thread whenever they
//! are caught while active.
//!
//! # Examples
//!
//! ```
//! wintrap::trap(vec![wintrap::Signal::CtrlC, wintrap::Signal::CloseWindow], |signal| {
//!     // handle signal here
//!     println!("Caught a signal: {:?}", signal);
//! }, || {
//!     // do work
//!     println!("Doing work");
//! }).unwrap();
//! ```
//!
//! # Caveats
//!
//! Please note that it is not possible to correctly trap Ctrl-C signals when
//! running programs via `cargo run`. You will have to run them directly via
//! the target directory after building.

#![feature(optin_builtin_traits)]
#[macro_use]
extern crate lazy_static;

mod windows;
use crossbeam_channel;
use std::collections::{HashMap, LinkedList};
use std::sync::{Arc, Mutex};
use std::thread;
use std::{error, fmt, process};
use winapi::shared::minwindef::{BOOL, DWORD, FALSE, LPARAM, LRESULT, TRUE, UINT, WPARAM};
use winapi::shared::windef::HWND;
use winapi::um::wincon::{CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT};
use winapi::um::winuser::{DefWindowProcW, WM_CLOSE, WM_QUIT};

/// Associates one or more [Signal]s to an callback function to be executed in
/// a dedicated thread while `body` is executing. A caveat of its usage is that
/// *only one thread* is ever able to trap signals throughout the entire
/// execution of your program. You are free to nest traps freely, however, only
/// the innermost signal handlers will be executed.
///
/// # Arguments
///
/// * `signals` - A vec of signals to trap during the execution of `body`.
///
/// * `handler` - The handler to execute whenever a signal is trapped. These
/// signals will be trapped and handled in the order that they are received in
/// a dedicated thread. The handler will *override* the default behavior of the
/// signal, in which most cases, is to end the process.
///
/// * `body` - The code to execute while the trap is active. The return value
/// will be used as the `Ok` value of the result of the trap call.
pub fn trap<RT: Sized>(
    signals: Vec<Signal>,
    handler: impl Fn(Signal) + Send + Sync + 'static,
    body: impl FnOnce() -> RT,
) -> Result<RT, Error> {
    let _trap_guard = Trap::new(signals, Arc::new(handler))?;
    Ok(body())
}

/// Represents one of several abstracted "signals" available to Windows
/// processes. A number of these signals may be associated with a single [trap]
/// call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// `SetConsoleCtrlHandler`-generated `CTRL_C_EVENT`. Equivalent to
    /// `SIGINT` on Unix. It is typically generated by the user pressing Ctrl+C
    /// in the console. However, the Restart Manager may also trigger this
    /// signal; see the
    /// [MSDN](https://docs.microsoft.com/en-us/windows/desktop/RstMgr/guidelines-for-applications)
    /// documentation for more details.
    CtrlC,

    /// `SetConsoleCtrlHandler`-generated `CTRL_BREAK_EVENT`. Roughly analagous
    /// to `SIGQUIT` on Unix. It is generated by the user pressing Ctrl+Break
    /// in the console.
    CtrlBreak,

    /// `SetConsoleCtrlHandler`-generated `CTRL_CLOSE_EVENT`. Roughly analagous
    /// to `SIGHUP` on Unix. It is generated by the user closing the console
    /// window.
    CloseConsole,

    /// A `WM_CLOSE` Window message. Roughly analagous to `SIGTERM` on Unix. It
    /// is generated by sending WM_CLOSE to the top-level windows in the
    /// process, which is done by [std::process::Child::kill()] and the Windows
    /// command line tool `taskkill`, among others.
    CloseWindow,
}

impl Signal {
    fn from_console_ctrl_event(event: DWORD) -> Option<Self> {
        match event {
            CTRL_C_EVENT => Some(Signal::CtrlC),
            CTRL_BREAK_EVENT => Some(Signal::CtrlBreak),
            CTRL_CLOSE_EVENT => Some(Signal::CloseConsole),
            _ => None,
        }
    }

    fn from_window_message(msg: UINT, wparam: WPARAM, _lparam: LPARAM) -> Option<Self> {
        if msg == WM_CLOSE {
            Some(Signal::CloseWindow)
        } else if msg == *WM_CONSOLE_CTRL {
            Signal::from_console_ctrl_event(wparam as DWORD)
        } else {
            None
        }
    }
}

/// An error that may potentially be generated by [trap]. These errors will
/// rarely ever be produced, and you can unwrap `Result`s safely in most cases.
#[derive(Debug)]
pub enum Error {
    /// An error setting the console control handler. The DWORD is the Windows
    /// error code; see the [MSDN
    /// documentation](https://docs.microsoft.com/en-us/windows/console/setconsolectrlhandler)
    /// for details.
    SetConsoleCtrlHandler(DWORD),

    /// An error occurred when creating a window or registering its window
    /// class. The DWORD is the Windows error code; see the MSDN documentation
    /// on
    /// [RegisterClassW](https://docs.microsoft.com/en-us/windows/desktop/api/winuser/nf-winuser-registerclassw)
    /// and
    /// [CreateWindowExW](https://docs.microsoft.com/en-us/windows/desktop/api/winuser/nf-winuser-createwindowexw)
    /// for more details.
    CreateWindow(DWORD),
}

impl error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::SetConsoleCtrlHandler(code) => write!(
                f,
                "Error setting console control handler: {}",
                windows::format_error(*code).unwrap()
            ),
            Error::CreateWindow(code) => write!(
                f,
                "Error creating Window: {}",
                windows::format_error(*code).unwrap()
            ),
        }
    }
}

lazy_static! {
    static ref WM_CONSOLE_CTRL: UINT =
        windows::register_window_message("WINSIG_WM_CONSOLE_CTRL").unwrap();
    static ref TRAP_STACK: Mutex<TrapStack> = Mutex::new(TrapStack::new());
    static ref TRAP_OWNER_THREAD_ID: thread::ThreadId = thread::current().id();
}

struct Trap {
    signals: Vec<Signal>,
}

impl Trap {
    fn new(
        signals: Vec<Signal>,
        handler: Arc<dyn Fn(Signal) + Send + Sync + 'static>,
    ) -> Result<Self, Error> {
        assert_eq!(*TRAP_OWNER_THREAD_ID, thread::current().id());
        let mut trap_stack = TRAP_STACK.lock().unwrap();
        trap_stack.push_trap(signals.as_slice(), handler)?;
        Ok(Trap { signals })
    }
}

impl Drop for Trap {
    fn drop(&mut self) {
        let mut trap_stack = TRAP_STACK.lock().unwrap();
        trap_stack.pop_trap(self.signals.as_ref());
    }
}

impl !Send for Trap {}
impl !Sync for Trap {}

type TrapCallbacks = HashMap<Signal, LinkedList<Arc<dyn Fn(Signal) + Send + Sync + 'static>>>;

struct TrapStack {
    num_traps: usize,
    trap_thread_data: Option<TrapThreadData>,
    callbacks: TrapCallbacks,
}

impl TrapStack {
    fn new() -> TrapStack {
        TrapStack {
            num_traps: 0,
            trap_thread_data: None,
            callbacks: HashMap::new(),
        }
    }

    fn increment_trap_count(&mut self) -> Result<(), Error> {
        self.num_traps += 1;
        if self.num_traps == 1 {
            // Initialize the active trap data
            self.trap_thread_data = Some(TrapThreadData::new()?);
        }
        Ok(())
    }

    fn decrement_trap_count(&mut self) {
        self.num_traps -= 1;
        if self.num_traps == 0 {
            // Drop the active trap data
            self.trap_thread_data = None;
        }
    }

    fn push_trap(
        &mut self,
        signals: &[Signal],
        handler: Arc<dyn Fn(Signal) + Send + Sync + 'static>,
    ) -> Result<(), Error> {
        self.increment_trap_count()?;
        for signal in signals.iter() {
            self.callbacks
                .entry(*signal)
                .or_insert_with(LinkedList::new)
                .push_back(handler.clone());
        }
        Ok(())
    }

    fn pop_trap(&mut self, signals: &[Signal]) {
        self.decrement_trap_count();
        for signal in signals.iter() {
            let callbacks = self.callbacks.get_mut(signal).unwrap();
            callbacks.pop_back().unwrap();
            if callbacks.is_empty() {
                self.callbacks.remove(signal);
            }
        }
    }

    fn has_handler_for(&self, signal: Signal) -> bool {
        self.callbacks.contains_key(&signal)
    }

    fn exit_if_only_window(&self) {
        if let Some(ref trap_thread_data) = self.trap_thread_data {
            // If we get a WM_CLOSE event and we don't have a handler for it, AND if
            // this process does not own any other windows, quit.
            struct EnumWindowsData {
                hwnd: HWND,
                process_id: DWORD,
            };
            let enum_windows_data = EnumWindowsData {
                hwnd: trap_thread_data.window_handle.hwnd,
                process_id: process::id(),
            };
            unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
                let enum_windows_data = &*(lparam as *const EnumWindowsData);
                if enum_windows_data.hwnd == hwnd {
                    TRUE
                } else {
                    let (_, process_id) = windows::get_window_thread_process_id(hwnd);
                    if enum_windows_data.process_id == process_id {
                        FALSE
                    } else {
                        TRUE
                    }
                }
            }
            // If we get through all windows during enumeration, then we didn't
            // find any other windows that we own.
            if !windows::enum_windows(
                enum_windows_proc,
                (&enum_windows_data as *const EnumWindowsData) as LPARAM,
            ) {
                process::exit(0);
            }
        } else {
            unreachable!();
        }
    }
}

struct TrapThreadData {
    thread: Option<thread::JoinHandle<()>>,
    thread_id: DWORD,
    window_handle: windows::WindowHandle,
}

impl TrapThreadData {
    fn new() -> Result<TrapThreadData, Error> {
        // Initialize custom window message, console handler, and thread
        windows::set_console_ctrl_handler(console_ctrl_handler, true)
            .map_err(Error::SetConsoleCtrlHandler)?;

        // Window message loop
        let (s, r) = crossbeam_channel::bounded(2);
        let thread = Some(thread::spawn(move || {
            s.send(windows::get_current_thread_id() as usize).unwrap();
            let mut window = windows::Window::new(window_proc).unwrap();
            s.send(window.hwnd as usize).unwrap();
            window
                .run_event_loop(|&msg| {
                    if let Some(signal) =
                        Signal::from_window_message(msg.message, msg.wParam, msg.lParam)
                    {
                        let trap_stack = TRAP_STACK.lock().unwrap();
                        if let Some(callback_list) = trap_stack.callbacks.get(&signal) {
                            callback_list.back().unwrap()(signal);
                        } else if msg.message == WM_CLOSE {
                            // Exit the process if we don't own any other windows.
                            trap_stack.exit_if_only_window();
                        }
                    }
                })
                .unwrap();
        }));
        let thread_id = r.recv().unwrap() as DWORD;
        let hwnd = r.recv().unwrap() as HWND;
        Ok(TrapThreadData {
            thread,
            thread_id,
            window_handle: windows::WindowHandle { hwnd },
        })
    }

    fn enqueue_ctrl_event(&self, event: DWORD) -> Result<(), DWORD> {
        windows::post_message(self.window_handle, *WM_CONSOLE_CTRL, event as WPARAM, 0)
    }
}

impl Drop for TrapThreadData {
    fn drop(&mut self) {
        windows::set_console_ctrl_handler(console_ctrl_handler, false).unwrap();
        windows::post_thread_message(self.thread_id, WM_QUIT, 0, 0).unwrap();
        self.thread.take().unwrap().join().unwrap();
    }
}

unsafe extern "system" fn console_ctrl_handler(event: DWORD) -> BOOL {
    match Signal::from_console_ctrl_event(event) {
        Some(signal) => {
            let trap_stack = TRAP_STACK.lock().unwrap();
            if trap_stack.has_handler_for(signal) {
                // A handler exists, so queue the signal to be handled in the
                // window thread
                match trap_stack.trap_thread_data {
                    Some(ref trap_thread_data) => {
                        match trap_thread_data.enqueue_ctrl_event(event) {
                            Ok(_) => TRUE,
                            Err(_) => FALSE,
                        }
                    }
                    None => FALSE,
                }
            } else {
                FALSE
            }
        }
        None => FALSE,
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Don't dare calling any user callbacks over the C function boundry. This
    // function should just simulate having processed the message by returning
    // the correct result. The actual processing happens in the callback to
    // `run_event_loop`.

    // Don't ever run the default handler for WM_CLOSE, as it destroys the
    // window.
    if msg == WM_CLOSE || msg == *WM_CONSOLE_CTRL {
        0
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nested_traps() {
        trap(
            vec![Signal::CtrlC, Signal::CloseWindow],
            |_| {},
            || {
                println!("Trap 1");
                trap(
                    vec![Signal::CtrlC, Signal::CtrlBreak],
                    |_| {},
                    || {
                        println!("Trap 2");
                    },
                )
                .unwrap();
            },
        )
        .unwrap();
    }

    #[test]
    fn test_trap_exit_and_reenter() {
        trap(
            vec![Signal::CtrlC],
            |_| {},
            || {
                println!("Trap 1");
            },
        )
        .unwrap();
        trap(
            vec![Signal::CtrlC],
            |_| {},
            || {
                println!("Trap 2");
            },
        )
        .unwrap();
    }
}
