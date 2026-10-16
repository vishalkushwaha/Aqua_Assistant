# Alarms, Reminders & Proactive Scheduler Engine (`core/src/modules/scheduler/`)

> **Code status (2026-10-08): written, not wired.** The scheduler engine has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. Register: `sim.scheduler` (simulated). Reminders that work are `core/src/reminders.rs` on the flows scheduler (`core/src/flows/sched/`).

---

## 1. Subsystem Architecture & Purpose

The **Scheduler Engine** drives Aqua's proactive intelligence, handling recurring cron jobs, alarms, calendar event reminders, and morning briefings.

---

## 2. Key Components & Workflow

1. **Cron & One-Shot Timer Queue (`cron_queue.rs`)**:
   - Manages recurring schedules (e.g. daily briefings at 8:00 AM) and custom relative timers (*"Remind me in 45 minutes to review the contract"*).
   - Operates on a Tokio timer wheel with sub-second precision and low power overhead.

2. **Proactive Daily Briefing Synthesizer (`daily_briefing.rs`)**:
   - Queries local calendar DB, unread sticky notes, social watchdog feeds, and local weather bridges.
   - Generates a concise natural language overview and dispatches it to Piper TTS for voice delivery.

3. **Persistent State & Alarm Ringing (`alarm_manager.rs`)**:
   - Alarms ring via Tauri UI modal + audio chime even if the app was minimized or screen locked.
   - All scheduled jobs are persisted in `aqua_core.db` so no reminders are lost across OS reboots.
