# Image Generation Module (`core/src/modules/image_generator/`)

> **Code status (2026-10-08): written, not wired.** `ImageEngine` has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. Flows draw images in `core/src/flows/image/`. There is no register entry.

---

## 1. Subsystem Architecture & Purpose

The **Image Generator Engine** handles user voice and text requests for synthesizing graphics, diagrams, and illustrations (e.g. *"Generate an image of a futuristic mechanical watch"* or *"Create a flowchart diagram for my project"*).

When web search fails to return an appropriate license-free image or when custom graphics are requested, Aqua invokes the local CPU image generator.

---

## 2. Key Components & Pipeline

1. **Prompt Sanitizer & Enhancer (`prompt_enhancer.rs`)**:
   - Refines natural language prompts into optimized diffusion prompts.
   - Appends quality tags (*"masterpiece, high detail, clean vectors"*) while filtering out disallowed terms.

2. **CPU ONNX SD / FLUX Generator (`adapter_bridge.rs`)**:
   - Interfaces with `adapters/image_generation_adapter/` utilizing INT8 quantized CPU diffusion pipelines (e.g. `Stable Diffusion v1.5 ONNX INT8` or `SD-Turbo CPU`).
   - Runs in isolated low-priority worker threads to ensure Voice Standby audio buffers never drop samples.

3. **Storage & UI Delivery (`storage.rs`)**:
   - Saves generated PNG/WebP images into `data/generated_images/<timestamp>_<hash>.png`.
   - Emits a WebSocket event (`IMAGE_GENERATED`) to render the preview directly in the Tauri UI window.

---

## 3. SLA & Resource Controls

- **Resolution**: Default $512 \times 512$ px (up-scalable via local Lanczos filter to $1024 \times 1024$ px).
- **RAM Ceiling**: Strictly constrained within the <200MB execution allocation by unloading diffusion UNet models immediately after tensor inference completes.
- **Generation Latency Target**: ~4–8 seconds on 8-core modern x86_64 / Apple Silicon CPU.
