# Multi-Modal Document & Vision Reader (`core/src/modules/document_reader/`)

> **Code status (2026-10-08): written, not wired.** `UniversalDocumentEngine` has tests, and nothing outside them calls it ([who calls what](../../../../documentation/competitive/refresh/10_DEAD_CODE_CALLERS.md)). The text below describes the design, not what runs in Aqua. The readers that run are the tools `doc.read` and `doc.search` (`adapters/native_tools/src/doc/`) and `screen.ocr` (`adapters/vision_adapter/src/screen/`). There is no register entry of its own.

Parses and comprehends mobile PDFs, scanned documents, mobile screenshots, diagrams, and image documentation.

## Components
- **PDF & Image Document Parser**: Renders vector & raster pages from mobile PDFs, receipts, invoices, and office documents.
- **OCR Engine**: Utilizes `PP-OCR` CPU in `adapters/vision_adapter/` for high-accuracy multi-lingual text extraction.
- **Vision Language Model (VLM)**: Utilizes `Moondream2 GGUF` CPU in `adapters/vision_adapter/` to understand visual layouts, mobile screenshots, and complex document charts.
- **Vector RAG Integration**: Extracts document text and embeds knowledge into local vector memory (`core/src/memory/`).

---

## Multi-Modal Vision Subsystem Specification (`adapters/vision_adapter/`)

The **Vision Subsystem** provides 4 distinct local computer vision capabilities running strictly on CPU (<300MB RAM peak):

### 1. Document & Receipt OCR Processing (`document_ocr.rs`)
- **Technology**: `PP-OCR v4` INT8 ONNX running via CPU SIMD vectorization.
- **Function**: Extracts structured text, tabular data, total amounts, line items, and invoice dates from receipts, mobile PDFs, and handwritten notes in $<120\text{ms}$.

### 2. Local Face Detection & Anti-Spoofing (`face_detector.rs`)
- **Technology**: Ultra-lightweight `SCRFD` / `YOLOv8-Face` ONNX CPU model (<10MB RAM).
- **Function**:
  - Detects human faces in webcam streams or uploaded images in $< 15\text{ms}$.
  - Computes 512-dimensional facial landmark embeddings (`ArcFace` ONNX).
  - Performs **Liveness / Anti-Spoofing Verification** (detects digital screen spoofing vs. real human presence) to complement Voice Biometrics.

### 3. Real-Time Object Detection & Bounding Box Extractor (`object_detector.rs`)
- **Technology**: `YOLOv8-Nano` INT8 ONNX CPU (<12MB RAM).
- **Function**: Identifies 80+ object classes (laptops, phones, books, ID cards, physical hardware, vehicles) with precise bounding box coordinates $(x, y, w, h)$ in $<25\text{ms}$.

### 4. Visual Scene & Image Understanding (`vlm_scene.rs`)
- **Technology**: `Moondream2` GGUF Vision Language Model on CPU.
- **Function**: Generates natural language descriptions of images, reads complex diagrams/flowcharts, and answers natural language questions about visual content (*"What is written on the whiteboard in this photo?"*).

---

## Universal Document Lifecycle Engine (`document_reader/doc_lifecycle.rs`)

Aqua provides a full **Read, Write, Edit, and New Document Generation Subsystem** supporting all major document formats (PDF, DOCX, XLSX, CSV, Markdown, JSON, TXT):

### 1. Multi-Format Document Reader (`doc_reader.rs`)
- **Supported Formats**: `.pdf`, `.docx`, `.xlsx`, `.csv`, `.md`, `.json`, `.txt`, `.pptx`.
- **Function**: Reads vector text, embedded images, tables, formulas, and metadata into a standardized `DocumentDOM` data structure.

### 2. Document Modifier & In-Place Editor (`doc_editor.rs`)
- **Function**: Performs targeted edits on existing documents (e.g., updating a section in a `.docx` file, modifying cells in an `.xlsx` spreadsheet, or appending pages to a `.pdf`).
- **Atomic Backup**: Creates a automatic backup copy (`.bak`) before executing modifications.

### 3. Brand-New Document Generator (`doc_generator.rs`)
- **Function**: Generates brand-new documents from natural language prompts:
  - **PDF Generator**: Compiles formatted PDF reports, resumes, and invoices with headers, footers, tables, and embedded images.
  - **DOCX Generator**: Generates formatted Word documents with styled headings, bullet lists, and custom fonts.
  - **XLSX / CSV Generator**: Generates structured Excel spreadsheets with formula calculations, column formatting, and data tables.
  - **Markdown & Code Docs**: Synthesizes structured technical documentation (`README.md`, API specs) with syntax highlighting.

---

## Interactive Data Summarization & Modification Prompt Workflow (`document_reader/interactive_summary.rs`)

When you ask Aqua: *"Tell me the total data in this CSV / document file"*, Aqua executes an **Analysis, Summary, and Interactive Modification Prompt Workflow**:

```
[ User Request: "Tell me the total data in sales.csv" ]
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 1: Fast File Parsing & Statistical Data Extraction                │
│   - Reads CSV / XLSX / PDF / JSON data via `doc_reader.rs`             │
│   - Computes row counts, column types, total sums, averages, nulls     │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 2: Dual Output Presentation (Voice Summary + UI Visual Table)     │
│   - UI: Renders interactive data summary card & preview table           │
│   - Voice TTS: "Your sales.csv contains 1,250 rows across 6 columns.   │
│                 The total revenue column equals $145,200."             │
└────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌────────────────────────────────────────────────────────────────────────┐
│ Step 3: Proactive Interactive Modification Prompt                      │
│   - Speaks prompt: "Would you like me to update, filter, add new data, │
│                     or modify any values in this file?"                │
│   - Binds listener to Voice STT & UI Modal                             │
└────────────────────────────────────────────────────────────────────────┘
                           │
             User Response?
              ├── "Add a new row for September" ──► `doc_editor.rs` updates file
              ├── "Filter rows where total > 500" ──► Generates new filtered CSV
              └── "No changes needed"            ──► Concludes task cleanly
```

### Key Workflow Features:
1. **Instant Statistical Analysis**: Automatically calculates total rows, total sums, column categories, missing values, and data ranges without writing code.
2. **Proactive Interactive Voice Prompt**: After summarizing the data, Aqua proactively asks you: *"Would you like me to change, update, add new rows, or modify anything in this file?"*
3. **Voice or UI Direct Editing**: Accepts spoken voice commands (*"Update the price column by 10%"*) or UI modal inputs to modify the file in place with automatic `.bak` backup protection.
