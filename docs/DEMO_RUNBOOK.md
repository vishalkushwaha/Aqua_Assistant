# Company Brain demo runbook

This runbook produces repeatable evidence for Track 03. It does not require or expose a cloud credential.

## Before recording

1. Install Docker Desktop, Node.js 18 or newer, and npm.
2. From the repository root, start the pinned local database:

   ```powershell
   docker compose -f docker-compose.company-brain.yml up -d
   docker compose -f docker-compose.company-brain.yml ps
   ```

3. Configure the desktop bridge:

   ```powershell
   Set-Location apps/aqua-desktop
   Copy-Item .env.example .env
   npm.cmd install
   npm.cmd run dev
   ```

   The supplied defaults use `127.0.0.1:6379`, graph `AquaCompanyBrain`, no password, and no TLS. They are for the loopback-only local container. Remote FalkorDB requires TLS and a deployment secret.

4. Open `http://127.0.0.1:1420`, then open **Company Brain**.
5. Keep FalkorDB Browser at `http://127.0.0.1:3000` ready to show the `AquaCompanyBrain` graph.

## Recording script

### 0:00-0:30 - problem and graph

State the problem: ownership, incidents, decisions, documents, and chats are disconnected in ordinary systems. Show the graph model in the root README, then show FalkorDB Browser.

### 0:30-1:15 - graph-dependent retrieval

Select **Prepare demo company**. Ask:

> Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?

Show that the response names Priya Raman, the Identity Platform, `DEC-014`, and both sources. Point to the relationship paths and source cards, not only the prose answer.

### 1:15-1:50 - graph-controlled action

Select **Approve graph-backed action**. Explain that the mutation first rechecks:

`Ticket -[:BELONGS_TO]-> Project <-[:OWNS]- Person`

Show the resulting `ASSIGNED_TO`, `ACTED_ON`, and `SELECTED_FROM_GRAPH` relationships in FalkorDB Browser.

### 1:50-2:25 - persistent memory and handoff

In the normal Aqua agent flow, open a session, remember the exchange, publish shared state, hand it to a second agent, and recall it. Show the resulting `Agent`, `Session`, `Question`, `Answer`, `StateRevision`, and `Handoff` records. Do not claim this step passed unless it was performed against the live graph used in the recording.

### 2:25-2:50 - dependency proof

Stop only the disposable database:

```powershell
Set-Location ../..
docker compose -f docker-compose.company-brain.yml stop falkordb
```

Ask the question again. Show the explicit FalkorDB error and that no fallback answer is produced. Restart it afterward:

```powershell
docker compose -f docker-compose.company-brain.yml start falkordb
```

### 2:50-3:00 - close

Name the entry as **Track 03 - Company Brain**. State that FalkorDB controls retrieval, source paths, action validation, and persistent memory.

## Reset and repeat

The **Prepare demo company** action is idempotent, so most rehearsals need no reset. For a clean demo tenant, run `demo/company-brain.reset.cypher` in FalkorDB Browser against `AquaCompanyBrain`, then select **Prepare demo company** again. This deletes only `tenant_id = 'demo'` records.

To remove the disposable database and its volume after the demo:

```powershell
docker compose -f docker-compose.company-brain.yml down --volumes
```

This is intentionally a manual cleanup step because it destroys the local demo database.

## Evidence checklist

- Database health is visible before the demo.
- At least two graph hops are visible in the answer path.
- Source titles and source URIs are visible.
- The assignment is shown before and after approval.
- Later-session or second-agent recall is shown if claimed.
- Database disconnection produces refusal, not a fabricated answer.
- No `.env`, credential, token, private record, or terminal history is visible.
- The uploaded video URL works in an incognito window.

## Before publishing the repository

- Create a fresh clone in a separate directory and run the documented desktop setup from that clone.
- Confirm `vendor` is not a Windows junction or a machine-local absolute dependency in the published source.
- Resolve the full core's `laya-candle` path dependency before claiming a reproducible full-core build. If it is not resolved, scope the reproducible submission path to the desktop plus the pinned FalkorDB container and state that limitation.
- Inspect every tracked file; exclude `.env`, tokens, databases, audit logs, build outputs, and local FalkorDB volumes.
- Run a history-aware secret scan. Revoke any credential that was ever published.
- Add the public repository URL and video URL to the submission form, and verify both without signing in.
- Confirm the team, timing, dataset-rights, AI-assistance, and participant-understanding attestations in the root README.
