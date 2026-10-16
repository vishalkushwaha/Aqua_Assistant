# IDE & Coding Tool MCP Orchestrator (`adapters/mcp_adapters/ide_mcp/`)

Provides bi-directional integration with IDEs, **Claude Code**, **OpenCode**, VS Code, and command-line development tools.

## Responsibilities
- **Inbound MCP**: Listens for commands dispatched from IDE extensions or external CLI coding tools to trigger Aqua agent actions.
- **Outbound Execution**: Drives external coding tools and terminal processes to inspect code, edit files, and execute build/test commands autonomously.
- **Error Learning**: Intercepts compiler traces and runtime errors, routing them to `core/src/reflection/` and the experience store (`core/src/memory/experience/`) to index verified fixes for future reuse.
