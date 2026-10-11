//! 控制端的核心逻辑：列出可选进程、把 hook DLL 注入目标进程、通过共享内存设置倍率。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, HANDLE, HWND, LPARAM, TRUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, VirtualAllocEx, VirtualFreeEx, FILE_MAP_ALL_ACCESS,
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetCurrentProcessId, IsWow64Process, OpenProcess,
    QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

const MAGIC: u32 = 0x5347_4541;
const SHARED_SIZE: usize = 256;
const MSG_LEN: usize = 128;

/// hook 位掩码里代表 Lua 包装器的那一位
pub const HOOK_LUA: u32 = 1 << 4;
/// 时间 API hook 的位
pub const HOOK_TIME_MASK: u32 = 0b1111;

pub const LUA_NONE: u32 = 0;
pub const LUA_ARMED: u32 = 1;
pub const LUA_RUNNING: u32 = 2;
pub const LUA_OK: u32 = 3;
pub const LUA_FAILED: u32 = 4;

/// 与 hook DLL 共享的内存布局，两边必须一致。
#[repr(C)]
struct Shared {
    magic: u32,
    hooks: AtomicU32,
    speed_bits: AtomicU64,
    tick_speed_bits: AtomicU64,
    lua_status: AtomicU32,
    lua_attempts: AtomicU32,
    msg: [u8; MSG_LEN],
}

/// 变速方式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 缩放时间 API 的返回值
    Time,
    /// 每帧多跑几次逻辑更新（LÖVE 引擎）
    Tick,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 指定虚拟键当前是否按下。
pub fn key_down(vk: u16) -> bool {
    vk != 0 && unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

/// 一个有可见窗口的进程。
#[derive(Clone, Debug)]
pub struct WindowProcess {
    pub pid: u32,
    /// exe 文件名
    pub name: String,
    pub title: String,
}

unsafe fn exe_path(pid: u32) -> Option<String> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
    CloseHandle(process);
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn exe_name(pid: u32) -> Option<String> {
    let path = exe_path(pid)?;
    Some(path.rsplit('\\').next().unwrap_or(&path).to_string())
}

/// 进程 exe 的版本特征：`<文件大小>-<修改时间的 Unix 秒数>`。游戏更新后会变，
/// 用来判断一份验证过的配置是否还对得上当前版本。
pub fn exe_build(pid: u32) -> Option<String> {
    let path = unsafe { exe_path(pid)? };
    let meta = std::fs::metadata(path).ok()?;
    let secs = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{}-{}", meta.len(), secs))
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam as *mut Vec<WindowProcess>);
    if IsWindowVisible(hwnd) == 0 {
        return TRUE;
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return TRUE;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 || pid == GetCurrentProcessId() || list.iter().any(|p| p.pid == pid) {
        return TRUE;
    }
    // 打不开的进程（权限更高）也注入不了，直接不列
    if let Some(name) = exe_name(pid) {
        list.push(WindowProcess {
            pid,
            name,
            title: String::from_utf16_lossy(&buf[..len as usize]),
        });
    }
    TRUE
}

/// 列出所有带可见窗口的进程（不含自己）。
pub fn list_window_processes() -> Vec<WindowProcess> {
    let mut list: Vec<WindowProcess> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_proc), &mut list as *mut _ as LPARAM);
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

unsafe fn open_shared(pid: u32) -> Option<*mut Shared> {
    let name = wide(&format!("Local\\SpeedGear2_{pid}"));
    let h = OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr());
    if h.is_null() {
        return None;
    }
    let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, SHARED_SIZE);
    // 视图会保持映射有效，句柄可以直接关掉
    CloseHandle(h);
    let p = view.Value as *mut Shared;
    if p.is_null() || (*p).magic != MAGIC {
        return None;
    }
    Some(p)
}

unsafe fn inject(pid: u32, dll: &Path) -> Result<(), String> {
    let path: Vec<u16> = dll.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let bytes = path.len() * 2;
    let process: HANDLE = OpenProcess(
        PROCESS_CREATE_THREAD
            | PROCESS_QUERY_INFORMATION
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE,
        0,
        pid,
    );
    if process.is_null() {
        return Err(format!(
            "打不开目标进程（错误码 {}）。如果游戏是以管理员身份运行的，本程序也需要以管理员身份运行",
            GetLastError()
        ));
    }
    let result = (|| {
        let mut wow64 = 0;
        if IsWow64Process(process, &mut wow64) != 0 && wow64 != 0 {
            return Err("目标是 32 位程序，目前只支持 64 位".to_string());
        }
        let mem = VirtualAllocEx(process, null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if mem.is_null() {
            return Err(format!("VirtualAllocEx 失败，错误码 {}", GetLastError()));
        }
        if WriteProcessMemory(process, mem, path.as_ptr() as *const c_void, bytes, null_mut()) == 0
        {
            return Err(format!("WriteProcessMemory 失败，错误码 {}", GetLastError()));
        }
        // kernel32 在同一会话的同位数进程里基址相同，可以直接用本进程的地址
        let load = GetProcAddress(
            GetModuleHandleW(wide("kernel32.dll").as_ptr()),
            b"LoadLibraryW\0".as_ptr(),
        )
        .ok_or("找不到 LoadLibraryW")?;
        let thread = CreateRemoteThread(
            process,
            null(),
            0,
            Some(std::mem::transmute(load)),
            mem,
            0,
            null_mut(),
        );
        if thread.is_null() {
            return Err(format!(
                "CreateRemoteThread 失败（错误码 {}），可能被安全软件拦截",
                GetLastError()
            ));
        }
        WaitForSingleObject(thread, 10_000);
        CloseHandle(thread);
        VirtualFreeEx(process, mem, 0, MEM_RELEASE);
        Ok(())
    })();
    CloseHandle(process);
    result
}

/// 与一个已注入的游戏进程的连接。
pub struct Session {
    pub pid: u32,
    process: HANDLE,
    shared: *mut Shared,
}

// 句柄和共享内存指针都可以跨线程使用
unsafe impl Send for Session {}

impl Session {
    /// 连接到进程；进程里还没有 hook DLL 时先注入 `hook_dll`。
    pub fn attach(pid: u32, hook_dll: &Path) -> Result<Session, String> {
        unsafe {
            let shared = match open_shared(pid) {
                Some(p) => p,
                None => {
                    if !hook_dll.exists() {
                        return Err(format!("找不到 {}", hook_dll.display()));
                    }
                    // 游戏会一直占用被加载的 DLL 文件。注入一份带时间戳的副本，
                    // 这样游戏运行期间原文件仍可更新。
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|e| e.to_string())?
                        .as_secs();
                    let copy = hook_dll.with_file_name(format!("speedgear_hook.{stamp}.loaded.dll"));
                    std::fs::copy(hook_dll, &copy).map_err(|e| format!("复制 DLL 失败: {e}"))?;
                    inject(pid, &copy)?;
                    let start = Instant::now();
                    loop {
                        if let Some(p) = open_shared(pid) {
                            break p;
                        }
                        if start.elapsed() > Duration::from_secs(5) {
                            return Err(
                                "注入后 5 秒内 DLL 没有完成初始化，可能被安全软件拦截".into()
                            );
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            };
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return Err(format!("OpenProcess 失败，错误码 {}", GetLastError()));
            }
            Ok(Session { pid, process, shared })
        }
    }

    /// 目标进程是否还在运行。
    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// 把 `mode` 对应的倍率设为 `speed`，另一种方式复位为 1。
    pub fn set_speed(&self, mode: Mode, speed: f64) {
        let (time, tick) = match mode {
            Mode::Time => (speed, 1.0),
            Mode::Tick => (1.0, speed),
        };
        unsafe {
            (*self.shared).speed_bits.store(time.to_bits(), Ordering::SeqCst);
            (*self.shared).tick_speed_bits.store(tick.to_bits(), Ordering::SeqCst);
        }
    }

    /// 已安装的 hook 位掩码。
    pub fn hooks(&self) -> u32 {
        unsafe { (*self.shared).hooks.load(Ordering::SeqCst) }
    }

    /// Lua 包装器状态，见 `LUA_*` 常量。
    pub fn lua_status(&self) -> u32 {
        unsafe { (*self.shared).lua_status.load(Ordering::SeqCst) }
    }

    /// Lua 包装器安装失败时的错误信息。
    pub fn lua_message(&self) -> String {
        let msg = unsafe { &(*self.shared).msg };
        let len = msg.iter().position(|&b| b == 0).unwrap_or(MSG_LEN);
        String::from_utf8_lossy(&msg[..len]).into_owned()
    }

    /// 该进程能否使用指定的变速方式。
    pub fn supports(&self, mode: Mode) -> bool {
        match mode {
            Mode::Time => self.hooks() & HOOK_TIME_MASK != 0,
            Mode::Tick => self.hooks() & HOOK_LUA != 0 && self.lua_status() == LUA_OK,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.process);
        }
    }
}
