//! 注入到游戏进程内的 DLL：让游戏变速。
//!
//! 倍率通过命名共享内存 `Local\SpeedGear2_<pid>` 由外部程序实时写入。
//!
//! 两种变速方式：
//! - 时间倍率：缩放时间 API 的返回值，适用于按真实时间推进的游戏。
//! - tick 倍率：进程里有 LuaJIT（LÖVE 引擎）时，往游戏的 Lua 环境里装一个
//!   `love.update` 包装器，每帧多调用几次逻辑更新。适用于每帧固定推进一步、
//!   丢弃多余时间的游戏（王国保卫战就是这样）。

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{BOOL, HMODULE, INVALID_HANDLE_VALUE, TRUE};
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleHandleW, GetProcAddress,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ,
    PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::{CreateThread, GetCurrentProcessId};

const MAGIC: u32 = 0x5347_4541; // "SGEA"
const SHARED_SIZE: u32 = 256;
const MSG_LEN: usize = 128;

const HOOK_QPC: u32 = 1 << 0;
const HOOK_TICK64: u32 = 1 << 1;
const HOOK_TICK: u32 = 1 << 2;
const HOOK_TIMEGETTIME: u32 = 1 << 3;
const HOOK_LUA: u32 = 1 << 4;

const LUA_NONE: u32 = 0;
const LUA_ARMED: u32 = 1;
const LUA_RUNNING: u32 = 2;
const LUA_OK: u32 = 3;
const LUA_FAILED: u32 = 4;
/// 非主 Lua 状态（工作线程）也会触发 hook，尝试次数设上限
const LUA_MAX_ATTEMPTS: u32 = 2000;

/// 与外部程序共享的内存布局，两边必须一致。
#[repr(C)]
struct Shared {
    magic: u32,
    /// 已成功安装的 hook 位掩码
    hooks: AtomicU32,
    /// 时间倍率，f64 的位模式
    speed_bits: AtomicU64,
    /// tick 倍率，f64 的位模式；由游戏内的 Lua 包装器直接读取
    tick_speed_bits: AtomicU64,
    /// Lua 包装器的安装状态，见 LUA_* 常量
    lua_status: AtomicU32,
    lua_attempts: AtomicU32,
    /// Lua 安装失败时的错误信息，以 0 结尾
    msg: [u8; MSG_LEN],
}

static SHARED: AtomicPtr<Shared> = AtomicPtr::new(null_mut());

fn current_speed() -> f64 {
    let p = SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return 1.0;
    }
    let s = f64::from_bits(unsafe { (*p).speed_bits.load(Ordering::Relaxed) });
    if s.is_finite() {
        s.clamp(0.05, 100.0)
    } else {
        1.0
    }
}

/// 一个被缩放的时钟。倍率变化时重设基准点，保证返回值连续且不回退。
struct Clock {
    state: Mutex<ClockState>,
}

struct ClockState {
    init: bool,
    base_real: i64,
    base_fake: i64,
    speed: f64,
    last: i64,
}

impl Clock {
    const fn new() -> Self {
        Clock {
            state: Mutex::new(ClockState {
                init: false,
                base_real: 0,
                base_fake: 0,
                speed: 1.0,
                last: i64::MIN,
            }),
        }
    }

    fn convert(&self, real: i64) -> i64 {
        let speed = current_speed();
        let mut s = match self.state.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if !s.init {
            s.init = true;
            s.base_real = real;
            s.base_fake = real;
            s.speed = speed;
        } else if speed != s.speed {
            s.base_fake += ((real - s.base_real) as f64 * s.speed) as i64;
            s.base_real = real;
            s.speed = speed;
        }
        let fake = s.base_fake + ((real - s.base_real) as f64 * s.speed) as i64;
        if fake > s.last {
            s.last = fake;
        }
        s.last
    }
}

static QPC_CLOCK: Clock = Clock::new();
static TICK_CLOCK: Clock = Clock::new();
static TGT_CLOCK: Clock = Clock::new();

static ORIG_QPC: AtomicUsize = AtomicUsize::new(0);
static ORIG_TICK64: AtomicUsize = AtomicUsize::new(0);
static ORIG_TGT: AtomicUsize = AtomicUsize::new(0);

type QpcFn = unsafe extern "system" fn(*mut i64) -> BOOL;
type Tick64Fn = unsafe extern "system" fn() -> u64;
type Tick32Fn = unsafe extern "system" fn() -> u32;

unsafe extern "system" fn hk_qpc(out: *mut i64) -> BOOL {
    let orig: QpcFn = std::mem::transmute(ORIG_QPC.load(Ordering::Relaxed));
    let mut real = 0i64;
    let r = orig(&mut real);
    if !out.is_null() {
        *out = QPC_CLOCK.convert(real);
    }
    r
}

unsafe extern "system" fn hk_tick64() -> u64 {
    let orig: Tick64Fn = std::mem::transmute(ORIG_TICK64.load(Ordering::Relaxed));
    TICK_CLOCK.convert(orig() as i64) as u64
}

unsafe extern "system" fn hk_tick() -> u32 {
    hk_tick64() as u32
}

unsafe extern "system" fn hk_timegettime() -> u32 {
    let orig: Tick32Fn = std::mem::transmute(ORIG_TGT.load(Ordering::Relaxed));
    TGT_CLOCK.convert(orig() as i64) as u32
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn find(module: &str, name: &[u8]) -> *mut c_void {
    let h = GetModuleHandleW(wide(module).as_ptr());
    if h.is_null() {
        return null_mut();
    }
    match GetProcAddress(h, name.as_ptr()) {
        Some(f) => f as *mut c_void,
        None => null_mut(),
    }
}

/// 安装一个 hook；`orig` 为 None 时不保存原函数地址。
unsafe fn install(target: *mut c_void, detour: *mut c_void, orig: Option<&AtomicUsize>) -> bool {
    if target.is_null() {
        return false;
    }
    let mut tramp: *mut c_void = null_mut();
    if minhook_sys::MH_CreateHook(target, detour, &mut tramp) != minhook_sys::MH_OK {
        return false;
    }
    // 必须在启用前写好原函数地址，否则 hook 生效瞬间会调用空指针
    if let Some(o) = orig {
        o.store(tramp as usize, Ordering::SeqCst);
    }
    minhook_sys::MH_EnableHook(target) == minhook_sys::MH_OK
}

// ---- LuaJIT / LÖVE ----

type LuaState = *mut c_void;
type PushNumberFn = unsafe extern "C" fn(LuaState, f64);

struct LuaApi {
    gettop: unsafe extern "C" fn(LuaState) -> i32,
    settop: unsafe extern "C" fn(LuaState, i32),
    checkstack: unsafe extern "C" fn(LuaState, i32) -> i32,
    loadbuffer: unsafe extern "C" fn(LuaState, *const u8, usize, *const u8) -> i32,
    pcall: unsafe extern "C" fn(LuaState, i32, i32, i32) -> i32,
    tolstring: unsafe extern "C" fn(LuaState, i32, *mut usize) -> *const u8,
}

static LUA_API: AtomicPtr<LuaApi> = AtomicPtr::new(null_mut());
static LUA_CHUNK: AtomicPtr<String> = AtomicPtr::new(null_mut());
static ORIG_PUSHNUMBER: AtomicUsize = AtomicUsize::new(0);

/// 在游戏的 Lua 环境里执行的安装脚本。返回 "ok" / "retry" / "err: ..."。
/// `retry` 表示当前不是主 Lua 状态（比如工作线程），换下一次调用再试。
fn lua_chunk(tick_speed_addr: usize) -> String {
    format!(
        r#"
if rawget(_G, "__speedgear_installed") then return "ok" end
local love = rawget(_G, "love")
if type(love) ~= "table" or type(love.update) ~= "function" or not love.graphics then
  return "retry"
end
local ok, ffi = pcall(require, "ffi")
if not ok then return "err: ffi unavailable" end
local speed = ffi.cast("const double*", 0x{tick_speed_addr:x}ULL)
local inner = love.update
local acc = 0
love.update = function(...)
  local s = speed[0]
  if not (s >= 0.05 and s <= 100) then s = 1 end
  acc = acc + s
  while acc >= 1 do
    acc = acc - 1
    inner(...)
  end
end
rawset(_G, "__speedgear_installed", true)
return "ok"
"#
    )
}

unsafe fn set_msg(shared: *mut Shared, text: &[u8]) {
    let n = text.len().min(MSG_LEN - 1);
    let msg = &raw mut (*shared).msg as *mut u8;
    std::ptr::copy_nonoverlapping(text.as_ptr(), msg, n);
    *msg.add(n) = 0;
}

/// 在游戏线程持有的 Lua 状态上尝试安装包装器，返回新的 LUA_* 状态。
unsafe fn try_install_lua(l: LuaState, shared: *mut Shared) -> u32 {
    let api = &*LUA_API.load(Ordering::Relaxed);
    let chunk = &*LUA_CHUNK.load(Ordering::Relaxed);
    if (api.checkstack)(l, 4) == 0 {
        return LUA_ARMED;
    }
    let top = (api.gettop)(l);
    let mut failed =
        (api.loadbuffer)(l, chunk.as_ptr(), chunk.len(), b"=speedgear\0".as_ptr()) != 0;
    if !failed {
        failed = (api.pcall)(l, 0, 1, 0) != 0;
    }
    let mut len = 0usize;
    let p = (api.tolstring)(l, -1, &mut len);
    let text: &[u8] = if p.is_null() {
        b"err: no result"
    } else {
        std::slice::from_raw_parts(p, len)
    };
    let status = if failed {
        set_msg(shared, text);
        LUA_FAILED
    } else if text == b"ok" {
        LUA_OK
    } else if text == b"retry" {
        if (*shared).lua_attempts.fetch_add(1, Ordering::Relaxed) >= LUA_MAX_ATTEMPTS {
            set_msg(shared, b"err: main lua state not found");
            LUA_FAILED
        } else {
            LUA_ARMED
        }
    } else {
        set_msg(shared, text);
        LUA_FAILED
    };
    (api.settop)(l, top);
    status
}

/// `lua_pushnumber` 每帧都会被引擎在主线程调用，借它拿到 lua_State。
unsafe extern "C" fn hk_pushnumber(l: LuaState, n: f64) {
    let shared = SHARED.load(Ordering::Relaxed);
    if !shared.is_null() && (*shared).lua_status.load(Ordering::Relaxed) == LUA_ARMED {
        let status = &(*shared).lua_status;
        // 安装脚本自身也会触发 lua_pushnumber，RUNNING 状态挡住重入和其他线程
        if status
            .compare_exchange(LUA_ARMED, LUA_RUNNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let result = try_install_lua(l, shared);
            status.store(result, Ordering::SeqCst);
        }
    }
    let orig: PushNumberFn = std::mem::transmute(ORIG_PUSHNUMBER.load(Ordering::Relaxed));
    orig(l, n)
}

unsafe fn install_lua(shared: *mut Shared) -> bool {
    let module = "lua51.dll";
    macro_rules! sym {
        ($name:literal) => {{
            let p = find(module, concat!($name, "\0").as_bytes());
            if p.is_null() {
                return false;
            }
            std::mem::transmute(p)
        }};
    }
    let api = Box::new(LuaApi {
        gettop: sym!("lua_gettop"),
        settop: sym!("lua_settop"),
        checkstack: sym!("lua_checkstack"),
        loadbuffer: sym!("luaL_loadbuffer"),
        pcall: sym!("lua_pcall"),
        tolstring: sym!("lua_tolstring"),
    });
    LUA_API.store(Box::into_raw(api), Ordering::SeqCst);
    let addr = &(*shared).tick_speed_bits as *const AtomicU64 as usize;
    LUA_CHUNK.store(Box::into_raw(Box::new(lua_chunk(addr))), Ordering::SeqCst);
    (*shared).lua_status.store(LUA_ARMED, Ordering::SeqCst);
    install(
        find(module, b"lua_pushnumber\0"),
        hk_pushnumber as *mut c_void,
        Some(&ORIG_PUSHNUMBER),
    )
}

/// 早期版本的 DLL 是否已经在本进程里装过时间 hook（开发期重复注入时避免叠加）。
unsafe fn legacy_time_hooks_present() -> bool {
    let name = wide(&format!("Local\\SpeedGear_{}", GetCurrentProcessId()));
    !OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()).is_null()
}

unsafe fn install_time_hooks() -> u32 {
    let mut hooks = 0u32;
    // kernel32 的 QueryPerformanceCounter 最终都走到 ntdll，hook 这里覆盖最全
    let mut qpc = find("ntdll.dll", b"RtlQueryPerformanceCounter\0");
    if qpc.is_null() {
        qpc = find("kernel32.dll", b"QueryPerformanceCounter\0");
    }
    if install(qpc, hk_qpc as *mut c_void, Some(&ORIG_QPC)) {
        hooks |= HOOK_QPC;
    }
    if install(
        find("kernel32.dll", b"GetTickCount64\0"),
        hk_tick64 as *mut c_void,
        Some(&ORIG_TICK64),
    ) {
        hooks |= HOOK_TICK64;
        if install(find("kernel32.dll", b"GetTickCount\0"), hk_tick as *mut c_void, None) {
            hooks |= HOOK_TICK;
        }
    }
    if install(
        find("winmm.dll", b"timeGetTime\0"),
        hk_timegettime as *mut c_void,
        Some(&ORIG_TGT),
    ) {
        hooks |= HOOK_TIMEGETTIME;
    }
    hooks
}

unsafe fn create_shared() -> *mut Shared {
    let name = wide(&format!("Local\\SpeedGear2_{}", GetCurrentProcessId()));
    let h = CreateFileMappingW(
        INVALID_HANDLE_VALUE,
        null(),
        PAGE_READWRITE,
        0,
        SHARED_SIZE,
        name.as_ptr(),
    );
    if h.is_null() {
        return null_mut();
    }
    let view = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, SHARED_SIZE as usize);
    view.Value as *mut Shared
}

unsafe extern "system" fn init_thread(_: *mut c_void) -> u32 {
    let shared = create_shared();
    if shared.is_null() {
        return 1;
    }
    (*shared).speed_bits.store(1.0f64.to_bits(), Ordering::SeqCst);
    (*shared).tick_speed_bits.store(1.0f64.to_bits(), Ordering::SeqCst);
    SHARED.store(shared, Ordering::SeqCst);

    if minhook_sys::MH_Initialize() != minhook_sys::MH_OK {
        return 2;
    }

    let mut hooks = 0u32;
    if !legacy_time_hooks_present() {
        hooks |= install_time_hooks();
    }
    if install_lua(shared) {
        hooks |= HOOK_LUA;
    } else {
        (*shared).lua_status.store(LUA_NONE, Ordering::SeqCst);
    }

    (*shared).hooks.store(hooks, Ordering::SeqCst);
    // magic 最后写，外部程序以此判断初始化完成
    (*shared).magic = MAGIC;
    0
}

#[no_mangle]
unsafe extern "system" fn DllMain(module: HMODULE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        DisableThreadLibraryCalls(module);
        // 不在 loader lock 内装 hook，另起线程
        CreateThread(null(), 0, Some(init_thread), null(), 0, null_mut());
    }
    TRUE
}
