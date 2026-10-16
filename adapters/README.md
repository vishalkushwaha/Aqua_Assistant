# Open-Source Integration Layer (Adapters)

This directory houses wrapper interfaces and adapters for integrating open-source libraries and APIs.

## Workspace crates

| Directory | Crate | README |
| :--- | :--- | :--- |
| `audio_adapter/` | `aqua-audio-adapter` | [README.md](audio_adapter/README.md) |
| `vision_adapter/` | `aqua-vision-adapter` | [README.md](vision_adapter/README.md) |
| `mcp_adapters/` | `aqua-mcp-adapters` | [README.md](mcp_adapters/README.md) |
| `device_monitor/` | `aqua-device-monitor` | Device connection and battery monitoring, see [DEVICES.md](../documentation/DEVICES.md) |
| `orb_placement/` | `aqua-orb-placement` | Where the voice orb sits so it does not cover your work (pure logic; the Windows side is in the desktop shell), see [ORB_POSITION.md](../documentation/ORB_POSITION.md) |
| `tool_host/` | `aqua-tool-host` | The one place every host tool call goes through: validate, decide, ask, run, clean, write to the ledger. The agent's browser and desktop calls go through it only with `AQUA_DOOR=on` (by default they take their own routes and are only compared). See [TOOLS.md](../documentation/TOOLS.md) and [ONE_DOOR_PLAN.md](../documentation/ONE_DOOR_PLAN.md) |
| `native_tools/` | `aqua-native-tools` | The tools themselves (files, terminal, git, web, ssh, databases, containers, cloud, forges, monitoring, documents, clipboard). See [TOOLS_REFERENCE.md](../documentation/TOOLS_REFERENCE.md) |

Every file of every crate and its role is in [SOURCE_INVENTORY.md](../documentation/SOURCE_INVENTORY.md); the files that are written and never called (the vision engine stub, the social watchdog, the standby loop) are marked there.

Vendor sources live under [`vendor/`](../vendor/) — see [`documentation/VENDOR_SUBMODULES.md`](../documentation/VENDOR_SUBMODULES.md).

## Rules
1. Headless integration only: Do not import or bundle open-source UI components directly.
2. Maintain clean abstraction boundaries so third-party dependencies can be swapped without affecting the core system.
3. Validate licensing compliance before incorporating dependencies.
