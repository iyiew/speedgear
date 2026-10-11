//! 游戏配置档：每个游戏一份，记录变速方式、默认倍率、是否自动连接。
//!
//! 内置配置编进程序里，只有它们能带"已验证"标记；用户配置放在配置目录的
//! `profiles` 下，同一个游戏有用户配置时以用户的为准，但验证标记永远来自内置配置。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use speedgear_core::Mode;

const BUILTIN: &[&str] = &[
    include_str!("../profiles/kingdom-rush-genesis.json"),
    include_str!("../profiles/kingdom-rush-vengeance.json"),
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Verified {
    /// 验证时游戏 exe 的版本特征，见 `speedgear_core::exe_build`
    pub build: String,
    pub date: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Profile {
    pub exe: String,
    #[serde(default)]
    pub name: String,
    /// "tick" 或 "time"
    pub mode: String,
    #[serde(default = "default_speed")]
    pub speed: f64,
    #[serde(default)]
    pub verified: Option<Verified>,
    /// 检测到游戏启动时自动连接
    #[serde(default)]
    pub auto_attach: bool,
}

fn default_speed() -> f64 {
    3.0
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Time => "time",
        Mode::Tick => "tick",
    }
}

pub fn parse_mode(name: &str) -> Option<Mode> {
    match name {
        "time" => Some(Mode::Time),
        "tick" => Some(Mode::Tick),
        _ => None,
    }
}

pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Time => "时间倍率",
        Mode::Tick => "逻辑帧倍率",
    }
}

pub fn builtin() -> Vec<Profile> {
    BUILTIN
        .iter()
        .map(|text| serde_json::from_str(text).expect("内置配置格式错误"))
        .collect()
}

fn same_exe(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// 读取用户配置目录下的所有配置，格式有误的文件跳过。
pub fn load_user(dir: &Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

fn file_path(dir: &Path, exe: &str) -> PathBuf {
    let safe: String = exe
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    dir.join(format!("{safe}.json"))
}

/// 保存用户配置。验证信息不属于用户，不写入。
pub fn save_user(dir: &Path, profile: &Profile) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut p = profile.clone();
    p.verified = None;
    let text = serde_json::to_string_pretty(&p).map_err(std::io::Error::other)?;
    std::fs::write(file_path(dir, &p.exe), text)
}

pub struct Resolved {
    /// 生效的配置（用户的优先）
    pub profile: Profile,
    /// 同一个游戏的内置配置，验证标记从这里来
    pub builtin: Option<Profile>,
}

pub fn resolve(exe: &str, user: &[Profile], builtin: &[Profile]) -> Option<Resolved> {
    let u = user.iter().find(|p| same_exe(&p.exe, exe));
    let b = builtin.iter().find(|p| same_exe(&p.exe, exe));
    let profile = u.or(b)?.clone();
    Some(Resolved {
        profile,
        builtin: b.cloned(),
    })
}

/// 所有游戏各自生效的配置。
pub fn effective(user: &[Profile], builtin: &[Profile]) -> Vec<Profile> {
    let mut out: Vec<Profile> = user.to_vec();
    for b in builtin {
        if !out.iter().any(|p| same_exe(&p.exe, &b.exe)) {
            out.push(b.clone());
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Verification {
    /// 内置配置验证过这种方式，且游戏版本一致
    Verified,
    /// 验证过，但游戏版本变了
    Outdated,
    /// 没有验证过（自动识别、用户配置，或当前方式不是验证过的那种）
    None,
}

impl Verification {
    pub fn name(self) -> &'static str {
        match self {
            Verification::Verified => "verified",
            Verification::Outdated => "outdated",
            Verification::None => "none",
        }
    }
}

/// 当前用的变速方式 `mode` 和游戏版本 `build`，对内置配置的验证结果。
pub fn verification(builtin: Option<&Profile>, mode: Mode, build: Option<&str>) -> Verification {
    let Some((b, v)) = builtin.and_then(|b| b.verified.as_ref().map(|v| (b, v))) else {
        return Verification::None;
    };
    if parse_mode(&b.mode) != Some(mode) {
        return Verification::None;
    }
    if build == Some(v.build.as_str()) {
        Verification::Verified
    } else {
        Verification::Outdated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(exe: &str, mode: &str, verified: Option<&str>) -> Profile {
        Profile {
            exe: exe.into(),
            name: String::new(),
            mode: mode.into(),
            speed: 3.0,
            verified: verified.map(|b| Verified {
                build: b.into(),
                date: "2026-10-11".into(),
            }),
            auto_attach: false,
        }
    }

    #[test]
    fn builtin_profiles_parse_and_are_verified() {
        let all = builtin();
        let kr = all.iter().find(|p| p.exe == "Kingdom Rush Genesis.exe").unwrap();
        assert_eq!(kr.mode, "tick");
        assert!(kr.verified.is_some());
    }

    #[test]
    fn vengeance_profile_uses_time_mode_and_is_not_verified_yet() {
        let all = builtin();
        let kr = all.iter().find(|p| p.exe == "Kingdom Rush Vengeance.exe").unwrap();
        assert_eq!(kr.mode, "time");
        assert_eq!(kr.verified, None);
    }

    #[test]
    fn user_profile_wins_but_builtin_is_kept_for_verification() {
        let builtin = [profile("Game.exe", "tick", Some("1-2"))];
        let user = [profile("game.EXE", "time", None)];
        let r = resolve("Game.exe", &user, &builtin).unwrap();
        assert_eq!(r.profile.mode, "time");
        assert_eq!(r.builtin.unwrap().mode, "tick");
    }

    #[test]
    fn unknown_game_has_no_profile() {
        assert!(resolve("Other.exe", &[], &[profile("Game.exe", "tick", None)]).is_none());
    }

    #[test]
    fn verification_states() {
        let b = profile("Game.exe", "tick", Some("1-2"));
        assert_eq!(verification(Some(&b), Mode::Tick, Some("1-2")), Verification::Verified);
        assert_eq!(verification(Some(&b), Mode::Tick, Some("9-9")), Verification::Outdated);
        assert_eq!(verification(Some(&b), Mode::Tick, None), Verification::Outdated);
        // 换成没验证过的方式，绿勾就不该亮
        assert_eq!(verification(Some(&b), Mode::Time, Some("1-2")), Verification::None);
        assert_eq!(verification(None, Mode::Tick, Some("1-2")), Verification::None);
        let unverified = profile("Game.exe", "tick", None);
        assert_eq!(verification(Some(&unverified), Mode::Tick, Some("1-2")), Verification::None);
    }

    #[test]
    fn user_file_never_carries_verification() {
        let dir = std::env::temp_dir().join(format!("sg_test_{}", std::process::id()));
        let p = profile("My Game.exe", "tick", Some("1-2"));
        save_user(&dir, &p).unwrap();
        let loaded = load_user(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].verified, None);
        assert_eq!(loaded[0].exe, "My Game.exe");
    }

    #[test]
    fn effective_prefers_user_and_includes_builtin_only_games() {
        let builtin = [profile("A.exe", "tick", None), profile("B.exe", "tick", None)];
        let user = [profile("a.exe", "time", None)];
        let e = effective(&user, &builtin);
        assert_eq!(e.len(), 2);
        assert_eq!(e.iter().find(|p| p.exe.eq_ignore_ascii_case("a.exe")).unwrap().mode, "time");
    }
}
