//! Detach a private Windows launch console, preserving an attached parent terminal.
pub fn prepare_gui() {
    #[cfg(all(windows, feature = "gui"))]
    {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetConsoleWindow() -> *mut c_void;
            fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
            fn FreeConsole() -> i32;
        }
        let mut process = 0u32;
        // A one-entry buffer also reports when other processes share this console.
        // API failures leave it attached. FreeConsole detaches only our process.
        unsafe {
            if !GetConsoleWindow().is_null()
                && GetConsoleProcessList(&mut process, 1) == 1
                && process == std::process::id()
            {
                FreeConsole();
            }
        }
    }
}

/// Show startup failures even when the private launch console has been released.
pub fn show_error(message: &str) {
    #[cfg(all(windows, feature = "gui"))]
    {
        use std::ffi::c_void;
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(
                owner: *mut c_void,
                text: *const u16,
                title: *const u16,
                kind: u32,
            ) -> i32;
        }
        let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        let title: Vec<u16> = "memtest\0".encode_utf16().collect();
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
        }
    }
    #[cfg(not(all(windows, feature = "gui")))]
    let _ = message;
}
