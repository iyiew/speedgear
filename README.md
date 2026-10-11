# SpeedGear

**English** | [简体中文](README.zh-CN.md)

> Speed up your game — hold a key to fast-forward single-player Windows games. Built with Tauri + Rust.

SpeedGear is a game speed changer for Windows. Hold a key to fast-forward a single-player game and release it to go back to normal speed, or press a key once to toggle fast-forward on and off.

**Single-player games only.** Do not use it with online games that have anti-cheat; it can get your account banned.

## Features

- **Hold to speed up, or toggle.** Two independent hotkeys, selectable in the UI. Keys are not swallowed, and they work while the game is fullscreen.
- **Two speed-up methods:**
  - **Time scale** scales the values returned by `QueryPerformanceCounter`, `GetTickCount(64)` and `timeGetTime`. It works for games that advance with real time.
  - **Logic-tick scale** is for games built on the LÖVE (LuaJIT) engine; it runs the game's logic update several times per frame. Some games advance one fixed step per frame and throw away the extra time, so scaling the clock does nothing for them. This method handles that case.
- **Per-game profiles.** One JSON file per game stores the speed-up method, default speed and whether to connect automatically. A built-in profile marked "verified" means the author tested that method on that build of the game.
- The game returns to normal speed when you disconnect or close SpeedGear.

## Supported games

| Game | Method | Status |
|---|---|---|
| Kingdom Rush Genesis (Steam) | Logic-tick scale | ✓ Verified |
| Kingdom Rush Vengeance (Steam) | Time scale | ✓ Verified |

Other 64-bit single-player games are worth a try. SpeedGear detects which method is available, but there is no guarantee it works.

## Usage

1. Start the game, then start SpeedGear.
2. Pick the game in the list and click **Connect**.
3. Hold the speed-up key (default `F6`), or press the toggle key (default `F7`).

If the game runs as administrator, SpeedGear must run as administrator too.

## Build

You need the Rust toolchain (MSVC) and the WebView2 runtime (included in Windows 11).

```bash
cargo build --release
```

The output is in `target/release/`. `speedgear.exe` and `speedgear_hook.dll` must stay in the same folder.

| Directory | Purpose |
|---|---|
| `crates/hook` | The DLL injected into the game: hooks the time APIs, or installs an update wrapper into the game's Lua environment |
| `crates/core` | Process list, DLL injection, and setting the speed through shared memory |
| `crates/app` | Tauri UI, hotkey polling and profiles |

## Adding a game

Drop a JSON file into `%APPDATA%\com.speedgear.app\profiles\`:

```json
{
  "exe": "Game.exe",
  "name": "Display name",
  "mode": "time",
  "speed": 3.0,
  "auto_attach": false
}
```

- `mode`: `time` (time scale) or `tick` (logic-tick scale).
- `auto_attach`: when `true`, SpeedGear connects as soon as it sees the game start.
- User profiles cannot carry a verification mark; a `verified` field there is ignored. To contribute a built-in profile, open a pull request and say which game build you tested it on. Built-in profiles live in [`crates/app/profiles`](crates/app/profiles).

## Limitations

- Windows and 64-bit games only.
- Logic-tick scale needs LuaJIT in the game process. Music and sound effects are not sped up.
- Like any tool that injects a DLL into another process, it can trigger antivirus warnings. Decide for yourself whether to trust it and add an exclusion.
- After a game update, a verified profile may no longer apply; the UI then shows "version changed".

## Disclaimer

This project is not affiliated with any game's developer or publisher. Game names belong to their respective owners and are used only to describe compatibility. Use at your own risk.

## License

[MIT](LICENSE)
