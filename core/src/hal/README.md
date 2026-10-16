# Hardware & OS Abstraction Layer (`core/src/hal/`)

> **Code status (2026-10-08): written, not wired.** `SessionWatcher` (about 30 lines) has tests, and nothing outside them calls it ([who calls what](../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. Aqua runs on Windows only. There is no register entry.

The **Hardware Abstraction Layer (HAL)** decouples Project Aqua's core logic from platform-specific operating system APIs. It provides a unified, cross-platform Rust interface (`async trait OSHandler`) supporting all major operating systems.

---

## 1. Supported Operating Systems & Architecture Matrix

| Platform | Supported Architecture | Audio Driver | System Autostart | Application Packaging |
| :--- | :--- | :--- | :--- | :--- |
| **Windows 10 / 11** | `x86_64`, `aarch64` (ARM64) | WASAPI / DirectSound | Windows Registry (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) | NSIS Installer / MSIX Package |
| **macOS (Intel & M-Series)** | `x86_64`, `aarch64` (Apple Silicon M1–M4) | CoreAudio | `launchd` Agent (`~/Library/LaunchAgents/`) | DMG Bundle / Notarized `.app` |
| **Linux (Ubuntu, Debian, Arch, Fedora)** | `x86_64`, `aarch64` (ARM64) | ALSA / PulseAudio / PipeWire | `systemd --user` / XDG Autostart | AppImage / Flatpak |
| **Android 8.0+** | `aarch64`, `armv7` | Remote Agent Bridge | Android Foreground Service | ADB / Remote Agent APK |
| **iOS / iPadOS** | Remote Access | WebSocket Gateway | N/A | REST / WebSocket Web App Bridge |

---

## 2. Abstraction Modules

- `autostart.rs`: Cross-platform OS boot registration.
- `session_watcher.rs`: Listens for OS lock screen and fast user switching events.
- `audio_hal.rs`: Bridges `cpal` with platform native audio drivers.
- `clipboard_hal.rs`: Cross-platform system clipboard access.
- `notification_hal.rs`: Native desktop toast & system notification sender (Windows Notification Center, macOS Notification Center, Linux DBus notifications).
