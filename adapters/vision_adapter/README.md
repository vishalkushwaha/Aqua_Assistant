# Vision Adapter (`aqua-vision-adapter`)

> **Code status (2026-10-08).** The part of this crate that runs is `src/screen/`: a picture of the screen taken in memory and read with the text reader that comes with Windows, as the tools `screen.ocr` and `screen.find_text` (no model, no download; register `vision.screen_text`, partial). `VisionEngine` below is **written, not wired** and simulated: it returns a fixed invoice text and two fixed boxes whether or not the model files exist (register `desktop.vision`; [who calls what](../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes that engine and its design.

CPU-oriented document vision: OCR, layout/object hints, and VLM integration points for the document reader pipeline.

---

## Crate API

Primary type: **`VisionEngine`** (`src/vision_engine.rs`).

```rust
let engine = VisionEngine::new(workspace_root)?;
let ocr = engine.run_ocr_on_image(path)?;
let objects = engine.detect_objects(path)?;
```

---

## Expected model files

Resolved relative to the path passed to `VisionEngine::new`:

**Recommended:** `data/models/vision/` (used in `vision_test.rs`).

**Note:** `aqua-cli` currently passes the workspace root; place models in `data/models/vision/` and update CLI to match for consistency.

| File | Purpose |
| :--- | :--- |
| `pp_ocr.onnx` | PP-OCR v4 text detection/recognition |
| `yolov8_nano.onnx` | Layout / object bounding boxes |

Recommended layout (align with [`data/models/README.md`](../../data/models/README.md)):

```
data/models/vision/
├── pp_ocr.onnx
└── yolov8_nano.onnx
```

If files are missing, the engine logs a warning and uses **simulator mode** (deterministic placeholder text/boxes for tests).

Future: Moondream2 GGUF VLM paths documented in [`documentation/API_INTERCONNECT_AND_MODELS.md`](../../documentation/API_INTERCONNECT_AND_MODELS.md).

---

## Types

- `OcrResult` — `extracted_text`, `confidence`, `detected_lines`
- `DetectedObject` — `label`, `confidence`, `bbox`

---

## Tests

```bash
cargo test -p aqua-vision-adapter
```

---

## Related docs

- Document reader module: [`core/src/modules/document_reader/README.md`](../../core/src/modules/document_reader/README.md)
- MCP vision notes: [`adapters/mcp_adapters/README.md`](../mcp_adapters/README.md)
