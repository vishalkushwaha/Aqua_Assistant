# Reflection & Self-Correction Engine

Analyzes failures, proposes patches, and gates fixes before they enter solution memory.

---

## Module

| File | Type |
| :--- | :--- |
| `reflection_engine.rs` | `ReflectionEngine`, `CrashReport` |
| `mod.rs` | Public exports |

Design detail (log parser, 3-attempt budget, counterfactual validation): [`learning/README.md`](../../../learning/README.md) Steps 6.1–6.2.

---

## Integration

- Feeds verified fixes into [`../memory/solutions_db.rs`](../memory/solutions_db.rs)
- Used by TDD / code generator flows ([`../modules/code_generator/README.md`](../modules/code_generator/README.md))

---

## Tests

`core/tests/reflection_test.rs`

---

## Related

- Requirements traceability domain #20: [`documentation/REQUIREMENTS_TRACEABILITY.md`](../../../documentation/REQUIREMENTS_TRACEABILITY.md)
