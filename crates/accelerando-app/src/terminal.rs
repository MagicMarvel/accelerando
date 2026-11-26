//! ANSI styles for terminal output. Format table cells before applying styles.

use std::fmt::{self, Display};
use std::io::{self, IsTerminal};

#[derive(Clone, Copy)]
pub(crate) struct Palette(bool);

impl Palette {
    pub(crate) fn stdout() -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return Self(false);
        }
        let terminal = io::stdout().is_terminal();
        let forced = std::env::var_os("CLICOLOR_FORCE")
            .is_some_and(|value| !value.is_empty() && value != "0");
        // Even in forced mode, try to enable VT processing for a Windows console.
        Self((terminal && enable_ansi()) || forced)
    }

    pub(crate) fn heading(self, text: impl Display) -> impl Display {
        self.style("\x1b[1;94m", text)
    }

    pub(crate) fn profit(self, text: impl Display) -> impl Display {
        self.style("\x1b[36m", text)
    }

    pub(crate) fn loss(self, text: impl Display) -> impl Display {
        self.style("\x1b[35m", text)
    }

    pub(crate) fn risk(self, text: impl Display) -> impl Display {
        self.style("\x1b[33m", text)
    }

    pub(crate) fn pnl(self, value: f64, text: impl Display) -> impl Display {
        let code = if value > 0.0 {
            "\x1b[36m"
        } else if value < 0.0 {
            "\x1b[35m"
        } else {
            ""
        };
        self.style(code, text)
    }

    fn style<T: Display>(self, code: &'static str, value: T) -> Styled<T> {
        Styled {
            code: if self.0 { code } else { "" },
            value,
        }
    }
}

struct Styled<T> {
    code: &'static str,
    value: T,
}

impl<T: Display> Display for Styled<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code.is_empty() {
            self.value.fmt(f)
        } else {
            write!(f, "{}{}\x1b[0m", self.code, self.value)
        }
    }
}

#[cfg(not(windows))]
fn enable_ansi() -> bool {
    std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

#[cfg(windows)]
fn enable_ansi() -> bool {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(kind: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    }

    // SAFETY: GetStdHandle returns a borrowed handle. The mode pointer addresses a
    // writable u32, and the console APIs report failure for invalid handles.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        GetConsoleMode(handle, &mut mode) != 0
            && (mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0
                || SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0)
    }
}
