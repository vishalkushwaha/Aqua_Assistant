# Switch AI Integration: Development Runbook

This guide covers the development, testing, and operation of the **Switch AI Connector** for Aqua AI Assistant. It enables Aqua to operate as an active team member inside team chat channels (Mattermost, Slack, Discord, MS Teams) via [Switch AI](https://www.wemakedevs.org/hackathons/switch).

---

## 🛠️ Prerequisites

1. **Node.js** 18+ (tested on Node v20/v26).
2. **FalkorDB** instance (local Docker or remote TLS).
3. **Switch Bridge / Console** (bundled with Mattermost for local zero-config testing).

---

## 🚀 Quick Verification (Offline Simulation)

You can verify the complete end-to-end Switch collaborative room workflow without needing external API tokens, live Slack workspaces, or external network connectivity.

Run the local mock simulation from `apps/aqua-desktop`:

```powershell
cd apps/aqua-desktop
npm run switch:simulate
```

This starts an in-process mock Switch Bridge, registers Aqua, connects the WebSocket client, simulates an incident report in chat, signs off on the assignment, and verifies the resulting memory graph and agent handoff.

---

## 🧪 Running the Automated Test Suite

All Switch test suites run as part of the core project test suite:

```powershell
cd apps/aqua-desktop

# Run all unit, integration, and simulation tests
npm test

# Run TypeScript typechecks on Switch code
npm run lint:switch
```

### Covered Test Suites:
* **`tests/switchState.test.ts`**: Verifies message deduplication window, state loading/recovery, and atomic pending assignment tracking.
* **`tests/switchWorkflow.test.ts`**: Verifies incident question handling, mention stripping, multi-hop evidence formatting, 15-minute approval expiration, cancellation, and fail-closed safety.
* **`tests/switchClient.test.ts`**: Verifies WebSocket URL normalization, error response parsing, and registration payload schemas.
* **`tests/switchSimulation.test.ts`**: Runs the live multi-step chat-to-graph simulation.

---

## 🏢 Running with Live Switch Console & Mattermost

When testing with a real Switch Console or Switch Bridge:

1. **Configure Environment Variables**:
   Copy `apps/aqua-desktop/.env.example` to `apps/aqua-desktop/.env`:
   ```env
   # FalkorDB Database Connection
   FALKORDB_HOST=127.0.0.1
   FALKORDB_PORT=6379
   FALKORDB_GRAPH=AquaCompanyBrain

   # Switch Agent Protocol
   SWITCH_BRIDGE_URL=http://127.0.0.1:8080
   SWITCH_REGISTRATION_TOKEN=your_switch_bridge_token
   SWITCH_ROOM_IDS=room_incident_triage
   SWITCH_AGENT_NAME=aqua-company-brain
   SWITCH_INCIDENT_AGENT_NAME=release-agent
   ```

2. **Register the Agent**:
   ```powershell
   cd apps/aqua-desktop
   npm run switch:register
   ```
   *Note: Credentials are automatically stored in the gitignored `.switch-agent.json`.*

3. **Start the Agent Listener**:
   From the repository root, simply run:
   ```cmd
   run_switch_agent.bat
   ```
   Or via PowerShell:
   ```powershell
   .\scripts\run-switch-agent.ps1
   ```

4. **Import Room Template**:
   In Switch Console, import the template [`demo/switch-incident-triage.template.yaml`](../demo/switch-incident-triage.template.yaml).

---

## 💬 Chat Interaction Flow

1. **Teammate in Chat**:
   > `@aqua-company-brain Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?`

2. **Aqua Response**:
   > Priya Raman (Staff Engineer, Identity Engineering) owns the Identity Platform and should handle the OAuth production incident. Earlier decision DEC-014 (Use staged PKCE migration) affects the fix.
   > 
   > Evidence paths:
   > - INC-1042 → BELONGS_TO → Identity Platform → OWNS → Priya Raman
   > - DEC-014 → AFFECTS → Identity Platform → SUPPORTED_BY → SRC-ADR-014
   > 
   > Sources:
   > - SRC-ADR-014: ADR-014: OAuth PKCE migration (company://docs/ADR-014)
   > 
   > Proposed action: Assign INC-1042 to Priya Raman. Reply `approve assignment INC-1042` within 15 minutes to execute it, or `cancel assignment`.

3. **Teammate Sign-off**:
   > `approve assignment INC-1042`

4. **Aqua Confirmation**:
   > Assignment completed: INC-1042 was assigned to Priya Raman after explicit approval in Switch. Action ACT-9042. Handoff HANDOFF-101 is stored in the graph. @release-agent

5. **Memory Inspection**:
   > `show room memory`

---

## 🔒 Git Policy
As requested, all changes remain strictly local. No commits or branches are pushed to `origin/main` until explicitly authorized.
