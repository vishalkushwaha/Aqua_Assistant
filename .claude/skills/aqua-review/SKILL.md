---
name: aqua-review
description: Use when reviewing a change, a diff or a bug report in this repository, or when writing an open decision (D-n) for the user. The answer contract (surface, bug, root cause, provenance, ownership, regression test, residual risk) and the decision brief.
---

# Review a change or a bug report

Where this skill, a document and the code disagree, the code wins. Say how you know each claim: **measured** (you ran it, with the command), **read** (file and function) or **reported** (someone said so). A review is findings first, worst harm first, each with the line or the input that shows it. Do not edit a file you did not make; do not touch the running core, a secret or a gate file (`CLAUDE.md`).

## The answer contract

Answer in this order, a line or a short paragraph each. Where the evidence is missing write "unknown" or "not checked", never a guess.

1. **Surface.** Where a person or a caller meets it: the route, tool, screen, voice command, script or setting, and the exact input that shows it.
2. **The bug.** What happens, what should happen, who is affected. If you could not reproduce it, say so and what you ran.
3. **Root cause.** The code path from the surface to the fault, with `path::function`. If it is not found, write what to run or read next, not a theory.
4. **Provenance.** `git log -S<text> -- <path>` and `git blame` only nominate a commit. Write "introduced by <sha>" only after a diff against that commit's parent shows the behaviour was different before; otherwise write "unknown".
5. **Ownership.** Does the fix belong where the fault shows, or at the boundary that owns the rule (the one door `ToolHost::call`, the policy, the provider, the gateway, the UI contract)? A patch in a caller that hides a fault in the code it calls is a finding.
6. **Regression test.** The smallest seam that fails before the fix and passes after: a test of the pure function before an integration test. Name the file and the test. Say whether you ran it before and after, or only traced it by hand.
7. **Residual risk.** What the fix does not cover, what else calls the same code, what was not run (a build per crate is the proof, the editor is a hint), and every threshold, baseline file or fixture the change touched: a check that fails is reported, not moved.

For a diff also say: what it changes for the person using Aqua, what is behind a switch and what its default is, and which documents or the register (`documentation/capabilities.toml`) it must update in the same change.

## The decision brief

Every open decision of the user (D-n) is written as one brief, under one screen:

- **D-n and a title** in plain words.
- **Behaviour and who is affected.** What Aqua does today and what it would do, for whom (the person, the voice, the agent, an API caller).
- **Why now.** What waits on it, or what gets worse if it waits.
- **Proof.** Measurements, tests, the code path, each labelled measured, read or reported.
- **Trade-offs.** What each side gains and loses. Speed and accuracy come first for Aqua; saving CPU, GPU or RAM is not a goal. Also safety, effort and whether it can be undone.
- **Recommendation.** One choice and one reason.
- **Choices.** Lettered. Each says the exact thing that happens when it is picked (the switch and its default, the file, the command) and how to undo it. One choice is always "leave it as it is".

The briefs go in the handoff note under "Waiting for the user" (`.claude/commands/handoff.md`).
