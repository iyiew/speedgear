#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 变速齿轮的桌面界面：选游戏进程、设倍率和热键。
//!
//! 热键由后台线程轮询键盘状态实现，不拦截按键，游戏全屏时也有效。
//! 每个游戏的变速方式、倍率、是否自动连接由配置档决定，见 `profile`。

mod profile;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use speedgear_core::{
    exe_build, key_down, list_window_processes, Mode, Session, LUA_ARMED, LUA_RUNNING,
};
use tauri::{AppHandle, Emitter, Manager, State};

use profile::{mode_label, mode_name, parse_mode, Profile};

const VK_F6: u16 = 0x75;
const VK_F7: u16 = 0x76;

/// 保存到磁盘的全局设置。倍率只是没有配置档的游戏的默认值。
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    speed: f64,
    /// 按住加速的键（虚拟键码），0 表示不用
    hold_vk: u16,
    /// 切换加速的键（虚拟键码），0 表示不用
    toggle_vk: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            speed: 3.0,
            hold_vk: VK_F6,
            toggle_vk: VK_F7,
        }
    }
}

struct Inner {
    config: Config,
    config_dir: PathBuf,
    session: Option<Session>,
    /// 已连接游戏的 exe 文件名
    target: String,
    /// 已连接游戏的显示名
    game_name: String,
    /// 已连接游戏 exe 的版本特征
    build: Option<String>,
    /// 已连接游戏对应的内置配置（验证标记从这里来）
    builtin: Option<Profile>,
    mode: Mode,
    /// 当前生效的倍率
    speed: f64,
    auto_attach: bool,
    /// 配置与实际不符之类需要常驻显示的提示
    warning: String,
    toggled: bool,
    toggle_was_down: bool,
    active: bool,
    /// 用户手动断开的进程，不再自动连回去
    suppressed_pid: u32,
}

type Shared = Arc<Mutex<Inner>>;

/// 发给界面的状态快照。
#[derive(Serialize, Clone, PartialEq)]
struct Snapshot {
    attached: bool,
    pid: u32,
    target: String,
    game_name: String,
    mode: &'static str,
    time_supported: bool,
    tick_supported: bool,
    speed: f64,
    hold_vk: u16,
    toggle_vk: u16,
    /// 当前是否正在加速
    active: bool,
    /// "verified" / "outdated" / "none"
    verification: &'static str,
    /// 内置配置验证过的变速方式
    verified_mode: Option<&'static str>,
    verified_date: String,
    auto_attach: bool,
    warning: String,
}

impl Inner {
    fn profiles_dir(&self) -> PathBuf {
        self.config_dir.join("profiles")
    }

    fn snapshot(&self) -> Snapshot {
        let s = self.session.as_ref();
        let verified = self
            .builtin
            .as_ref()
            .and_then(|b| b.verified.as_ref().map(|v| (b, v)));
        Snapshot {
            attached: s.is_some(),
            pid: s.map_or(0, |s| s.pid),
            target: self.target.clone(),
            game_name: self.game_name.clone(),
            mode: mode_name(self.mode),
            time_supported: s.is_some_and(|s| s.supports(Mode::Time)),
            tick_supported: s.is_some_and(|s| s.supports(Mode::Tick)),
            speed: self.speed,
            hold_vk: self.config.hold_vk,
            toggle_vk: self.config.toggle_vk,
            active: self.active,
            verification: profile::verification(self.builtin.as_ref(), self.mode, self.build.as_deref())
                .name(),
            verified_mode: verified.and_then(|(b, _)| parse_mode(&b.mode)).map(mode_name),
            verified_date: verified.map(|(_, v)| v.date.clone()).unwrap_or_default(),
            auto_attach: self.auto_attach,
            warning: self.warning.clone(),
        }
    }

    fn save_config(&self) {
        let _ = std::fs::create_dir_all(&self.config_dir);
        if let Ok(text) = serde_json::to_string_pretty(&self.config) {
            let _ = std::fs::write(self.config_dir.join("config.json"), text);
        }
    }

    fn release(&mut self) {
        if let Some(s) = self.session.take() {
            if s.alive() {
                s.set_speed(self.mode, 1.0);
            }
        }
        self.toggled = false;
        self.active = false;
        self.speed = self.config.speed;
        self.auto_attach = false;
        self.builtin = None;
        self.build = None;
        self.warning.clear();
    }
}

#[derive(Serialize)]
struct ProcessItem {
    pid: u32,
    name: String,
    title: String,
    /// 有对应的配置档
    known: bool,
    /// 内置配置里有这个游戏的验证记录
    verified: bool,
}

#[tauri::command]
fn list_processes(state: State<Shared>) -> Vec<ProcessItem> {
    let dir = state.lock().unwrap().profiles_dir();
    let builtin = profile::builtin();
    let profiles = profile::effective(&profile::load_user(&dir), &builtin);
    let mut list: Vec<ProcessItem> = list_window_processes()
        .into_iter()
        .map(|p| ProcessItem {
            known: profiles.iter().any(|x| x.exe.eq_ignore_ascii_case(&p.name)),
            verified: builtin
                .iter()
                .any(|b| b.verified.is_some() && b.exe.eq_ignore_ascii_case(&p.name)),
            pid: p.pid,
            name: p.name,
            title: p.title,
        })
        .collect();
    // 已验证的排最前，其次是有配置档的，其余保持原来的顺序（按 exe 名排序）
    list.sort_by_key(|p| (!p.verified, !p.known));
    list
}

#[tauri::command]
fn get_state(state: State<Shared>) -> Snapshot {
    state.lock().unwrap().snapshot()
}

/// 连接游戏：注入、按配置档（没有就自动识别）选变速方式。
fn do_attach(state: &Shared, pid: u32, name: String) -> Result<Snapshot, String> {
    let dir = state.lock().unwrap().profiles_dir();
    let dll = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("speedgear_hook.dll");
    // 注入会等待游戏响应，不持锁进行
    let session = Session::attach(pid, &dll)?;
    // Lua 包装器要等游戏跑到可以安装的时候，LÖVE 游戏刚启动时可能要等一会儿
    let start = Instant::now();
    while matches!(session.lua_status(), LUA_ARMED | LUA_RUNNING)
        && start.elapsed() < Duration::from_secs(8)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let build = exe_build(pid);
    let resolved = profile::resolve(&name, &profile::load_user(&dir), &profile::builtin());

    let mut inner = state.lock().unwrap();
    inner.release();
    // 没有配置可循时：能用逻辑帧倍率就用它（时间倍率对这类游戏通常无效），否则时间倍率
    let detected = if session.supports(Mode::Tick) { Mode::Tick } else { Mode::Time };
    let mut warning = String::new();
    let (mode, speed, auto_attach, game_name) = match &resolved {
        Some(r) => {
            let p = &r.profile;
            let speed = if (0.05..=100.0).contains(&p.speed) { p.speed } else { inner.config.speed };
            let game_name = if p.name.is_empty() { name.clone() } else { p.name.clone() };
            match parse_mode(&p.mode) {
                Some(m) if session.supports(m) => (m, speed, p.auto_attach, game_name),
                _ => {
                    let wanted = match parse_mode(&p.mode) {
                        Some(m) => mode_label(m).to_string(),
                        None => p.mode.clone(),
                    };
                    warning = format!(
                        "配置指定“{wanted}”，但这次连接没法使用它，已改用“{}”。游戏可能已更新，或配置有误",
                        mode_label(detected)
                    );
                    (detected, speed, p.auto_attach, game_name)
                }
            }
        }
        None => (detected, inner.config.speed, false, name.clone()),
    };
    session.set_speed(mode, 1.0);
    inner.session = Some(session);
    inner.target = name;
    inner.game_name = game_name;
    inner.build = build;
    inner.builtin = resolved.and_then(|r| r.builtin);
    inner.mode = mode;
    inner.speed = speed;
    inner.auto_attach = auto_attach;
    inner.warning = warning;
    Ok(inner.snapshot())
}

#[tauri::command(async)]
fn attach(pid: u32, name: String, state: State<Shared>) -> Result<Snapshot, String> {
    do_attach(&state, pid, name)
}

#[tauri::command]
fn detach(state: State<Shared>) -> Snapshot {
    let mut inner = state.lock().unwrap();
    inner.suppressed_pid = inner.session.as_ref().map_or(0, |s| s.pid);
    inner.release();
    inner.snapshot()
}

#[tauri::command]
fn set_config(
    speed: f64,
    mode: String,
    hold_vk: u16,
    toggle_vk: u16,
    auto_attach: bool,
    state: State<Shared>,
) -> Result<Snapshot, String> {
    if !(0.05..=100.0).contains(&speed) {
        return Err("倍率需在 0.05 到 100 之间".into());
    }
    let mode = parse_mode(&mode).ok_or("未知的变速方式")?;
    let mut inner = state.lock().unwrap();
    inner.config.hold_vk = hold_vk;
    inner.config.toggle_vk = toggle_vk;
    inner.speed = speed;
    inner.mode = mode;
    if inner.session.is_some() {
        // 连着游戏时，倍率、方式、自动连接都属于这个游戏的配置档
        inner.auto_attach = auto_attach;
        inner.warning.clear();
        let p = Profile {
            exe: inner.target.clone(),
            name: inner.game_name.clone(),
            mode: mode_name(mode).to_string(),
            speed,
            verified: None,
            auto_attach,
        };
        profile::save_user(&inner.profiles_dir(), &p).map_err(|e| format!("保存配置失败: {e}"))?;
    } else {
        inner.config.speed = speed;
    }
    inner.save_config();
    Ok(inner.snapshot())
}

/// 后台线程：轮询热键，把当前应有的倍率写给游戏。
fn hotkey_loop(app: AppHandle, state: Shared) {
    let mut last: Option<Snapshot> = None;
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let snapshot = {
            let mut inner = state.lock().unwrap();
            if inner.session.as_ref().is_some_and(|s| !s.alive()) {
                inner.release();
            }
            if inner.session.is_some() {
                let toggle_down = key_down(inner.config.toggle_vk);
                if toggle_down && !inner.toggle_was_down {
                    inner.toggled = !inner.toggled;
                }
                inner.toggle_was_down = toggle_down;
                inner.active = inner.toggled || key_down(inner.config.hold_vk);
                let speed = if inner.active { inner.speed } else { 1.0 };
                if let Some(s) = &inner.session {
                    s.set_speed(inner.mode, speed);
                }
            }
            inner.snapshot()
        };
        if last.as_ref() != Some(&snapshot) {
            let _ = app.emit("state", &snapshot);
            last = Some(snapshot);
        }
    }
}

#[derive(Serialize, Clone)]
struct Notice {
    text: String,
    kind: &'static str,
}

/// 后台线程：没连着游戏时，发现配置档里开了自动连接的游戏就连上去。
fn auto_attach_loop(app: AppHandle, state: Shared) {
    // 自动连接失败过的进程，不再重试
    let mut failed: HashSet<u32> = HashSet::new();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let (dir, suppressed) = {
            let inner = state.lock().unwrap();
            if inner.session.is_some() {
                continue;
            }
            (inner.profiles_dir(), inner.suppressed_pid)
        };
        let wanted: Vec<Profile> = profile::effective(&profile::load_user(&dir), &profile::builtin())
            .into_iter()
            .filter(|p| p.auto_attach)
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let found = list_window_processes().into_iter().find(|p| {
            p.pid != suppressed
                && !failed.contains(&p.pid)
                && wanted.iter().any(|w| w.exe.eq_ignore_ascii_case(&p.name))
        });
        let Some(found) = found else { continue };
        let notice = match do_attach(&state, found.pid, found.name.clone()) {
            Ok(_) => Notice {
                text: format!("已自动连接 {}", found.name),
                kind: "ok",
            },
            Err(e) => {
                failed.insert(found.pid);
                Notice {
                    text: format!("自动连接 {} 失败：{e}", found.name),
                    kind: "err",
                }
            }
        };
        let _ = app.emit("notice", notice);
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let config = std::fs::read_to_string(config_dir.join("config.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Config>(&text).ok())
                .unwrap_or_default();
            let state: Shared = Arc::new(Mutex::new(Inner {
                speed: config.speed,
                config,
                config_dir,
                session: None,
                target: String::new(),
                game_name: String::new(),
                build: None,
                builtin: None,
                mode: Mode::Time,
                auto_attach: false,
                warning: String::new(),
                toggled: false,
                toggle_was_down: false,
                active: false,
                suppressed_pid: 0,
            }));
            app.manage(state.clone());
            let (h1, s1) = (app.handle().clone(), state.clone());
            std::thread::spawn(move || hotkey_loop(h1, s1));
            let handle = app.handle().clone();
            std::thread::spawn(move || auto_attach_loop(handle, state));
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关窗口时把游戏恢复到正常速度
            if let tauri::WindowEvent::Destroyed = event {
                let state = window.state::<Shared>();
                state.lock().unwrap().release();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_processes,
            get_state,
            attach,
            detach,
            set_config
        ])
        .run(tauri::generate_context!())
        .expect("启动失败");
}
