# SpeedGear 变速齿轮

[English](README.md) | **简体中文**

> Speed up your game — hold a key to fast-forward single-player Windows games. Built with Tauri + Rust.

变速齿轮：按住一个键让单机游戏加速，松开还原；也可以按一下切换加速，再按一下还原。

**仅适用于单机游戏。** 带反作弊的网络游戏不要使用，可能导致封号。

## 功能

- **按住加速 / 按一下切换**：两个热键各自独立，可在界面里选择，按键不会被拦截，游戏全屏时也有效。
- **两种变速方式**：
  - **时间倍率**：缩放 `QueryPerformanceCounter`、`GetTickCount(64)`、`timeGetTime` 的返回值，适用于按真实时间推进的游戏。
  - **逻辑帧倍率**：针对 LÖVE（LuaJIT）引擎的游戏，每帧多执行几次逻辑更新。有些游戏每帧只推进固定一步、丢弃多余的时间，这类游戏缩放时间没有效果，要用这种方式。
- **游戏配置档**：每个游戏一份 JSON，记录变速方式、默认倍率、是否自动连接。内置配置里带"已验证"标记的，表示作者在该游戏的该版本上实测有效。
- 关闭程序或断开连接时，游戏自动恢复正常速度。

## 已支持的游戏

| 游戏 | 变速方式 | 状态 |
|---|---|---|
| Kingdom Rush Genesis（王国保卫战 6：新启程，Steam） | 逻辑帧倍率 | ✓ 已验证 |
| Kingdom Rush Vengeance（王国保卫战：复仇，Steam） | 时间倍率 | 未验证 |

其他 64 位单机游戏可以直接试，程序会自动识别能用哪种方式，但不保证有效。

## 使用

1. 先启动游戏，再打开 SpeedGear。
2. 在列表里选中游戏，点"连接"。
3. 按住加速键（默认 `F6`），或按切换键（默认 `F7`）。

如果游戏是以管理员身份运行的，SpeedGear 也要以管理员身份运行。

## 构建

需要 Rust 工具链（MSVC）和 WebView2 运行时（Windows 11 自带）。

```bash
cargo build --release
```

产物在 `target/release/`，其中 `speedgear.exe` 和 `speedgear_hook.dll` 必须放在同一个目录。

| 目录 | 作用 |
|---|---|
| `crates/hook` | 注入游戏进程的 DLL，拦截时间 API，或往 Lua 环境里装更新包装器 |
| `crates/core` | 进程列表、DLL 注入、通过共享内存设置倍率 |
| `crates/app` | Tauri 界面、热键轮询、配置档 |

## 添加游戏

在 `%APPDATA%\com.speedgear.app\profiles\` 下放一个 JSON 文件：

```json
{
  "exe": "Game.exe",
  "name": "游戏显示名",
  "mode": "time",
  "speed": 3.0,
  "auto_attach": false
}
```

- `mode`：`time`（时间倍率）或 `tick`（逻辑帧倍率）。
- `auto_attach`：为 `true` 时，检测到游戏启动自动连接。
- 用户配置不能带验证标记，`verified` 字段会被忽略。想贡献内置配置，请提 PR，并说明在哪个游戏版本上测试过。内置配置在 [`crates/app/profiles`](crates/app/profiles)。

## 限制

- 只支持 Windows 和 64 位游戏。
- 逻辑帧倍率要求游戏进程里有 LuaJIT，音乐音效不会跟着变速。
- 这类程序通过向游戏进程注入 DLL 实现，杀毒软件可能误报，需要自行判断并加白名单。
- 游戏更新后，已验证的配置可能不再适用，界面上会显示"版本已变"。

## 声明

本项目与任何游戏的开发商、发行商无关。文中游戏名称归各自所有者所有，仅用于说明兼容性。使用风险自负。

## 许可

[MIT](LICENSE)
