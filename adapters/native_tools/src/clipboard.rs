//! The clipboard as two tools of the tool host (card R4-03): `clipboard.read` gives the text that is on it now, `clipboard.write` replaces it.
//!
//! The clipboard is where a person's passwords, a half-written message and the number they just copied wait, so both tools are held back:
//!
//! * **`clipboard.read` asks every time** (the decision is "always ask": an answer is for that one call and is never remembered for the session). What it returns is
//!   **private** (a run that holds it may not send things out without asking, like a run that read the screen) and **untrusted** (the host fences it as text from
//!   outside, masks anything in it that looks like a secret, and marks the run as having read outside text). It gives text only, cut at [`MAX_READ_CHARS`] characters.
//! * **`clipboard.write` asks** and shows the person what it would put there. It is a write, so a spoken request may not do it and a run that read outside text must be
//!   asked again whatever was approved before.
//!
//! Windows-MCP's own `Clipboard` tool stays banned by name (`windows_mcp/guard.rs`): this is the only way in, and it goes through the host. The tools are registered by
//! `core/src/tool_host.rs` only when `AQUA_CLIPBOARD_TOOLS=on`; the clipboard is not reachable by the agent otherwise.

use aqua_tool_host::{Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::sync::Arc;

/// Characters of the clipboard that are handed on; the rest is counted and not shown. (The host cuts what a tool says to the model at 6,000 characters, fence and
/// heading included, so a bigger number here would only have the host cut off the sentence that says how much was left out.)
pub const MAX_READ_CHARS: usize = 5_000;
/// The most characters `clipboard.write` puts on the clipboard.
pub const MAX_WRITE_CHARS: usize = 20_000;
/// How much of the text the question to the person quotes.
const PREVIEW_CHARS: usize = 60;

/// Where the text comes from and goes to: the clipboard of Windows in the real thing, a fixed one in a test.
pub trait ClipboardSource: Send + Sync {
    /// The text on the clipboard, or `None` when it holds none (a picture, files, nothing).
    fn read_text(&self) -> Result<Option<String>, String>;
    /// Replaces what is on the clipboard with `text`.
    fn write_text(&self, text: &str) -> Result<(), String>;
}

/// The clipboard of this computer. On a system other than Windows it says so and does nothing.
pub struct SystemClipboard;

impl ClipboardSource for SystemClipboard {
    fn read_text(&self) -> Result<Option<String>, String> {
        #[cfg(windows)]
        {
            win::read_text()
        }
        #[cfg(not(windows))]
        {
            Err("the clipboard tools work on Windows only".to_string())
        }
    }

    fn write_text(&self, text: &str) -> Result<(), String> {
        #[cfg(windows)]
        {
            win::write_text(text)
        }
        #[cfg(not(windows))]
        {
            let _ = text;
            Err("the clipboard tools work on Windows only".to_string())
        }
    }
}

pub struct ClipboardProvider {
    source: Arc<dyn ClipboardSource>,
}

impl ClipboardProvider {
    pub fn new(source: Arc<dyn ClipboardSource>) -> Self {
        Self { source }
    }

    /// The tools over this computer's own clipboard.
    pub fn system() -> Self {
        Self::new(Arc::new(SystemClipboard))
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, message)
}

/// The text a write is asked to put, checked: present, not empty, at most [`MAX_WRITE_CHARS`] characters.
fn text_to_write(args: &Value) -> Result<&str, ToolError> {
    let text = args.get("text").and_then(|v| v.as_str()).ok_or_else(|| invalid("text is the text to put on the clipboard, and it is missing"))?;
    let chars = text.chars().count();
    if chars == 0 {
        return Err(invalid("text is empty: there is nothing to put on the clipboard"));
    }
    if chars > MAX_WRITE_CHARS {
        return Err(invalid(format!("text is {chars} characters, and at most {MAX_WRITE_CHARS} can be put on the clipboard at once")));
    }
    Ok(text)
}

/// The start of a text for a question to the person: one line, no control characters, [`PREVIEW_CHARS`] characters at most.
fn preview(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let head: String = flat.chars().take(PREVIEW_CHARS).collect();
    if flat.chars().count() > PREVIEW_CHARS {
        format!("{head}...")
    } else {
        head
    }
}

impl ToolProvider for ClipboardProvider {
    fn id(&self) -> &str {
        "clipboard"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        vec![
            ToolManifest {
                name: "clipboard.read".into(),
                title: "clipboard.read".into(),
                description: "Read the text on the clipboard now. It asks every time, and what it returns is private.".into(),
                classes: vec![PermClass::Read],
                default_decision: Decision::AlwaysAsk,
                input_schema: json!({"type": "object", "properties": {}, "required": []}),
                timeout_default_ms: 5_000,
                timeout_max_ms: 10_000,
                annotations: Annotations { read_only: true, destructive: false, idempotent: false, open_world: false },
                untrusted_output: true,
            },
            ToolManifest {
                name: "clipboard.write".into(),
                title: "clipboard.write".into(),
                description: "Replace the text on the clipboard. It asks first, and what was on the clipboard is lost.".into(),
                classes: vec![PermClass::Write],
                default_decision: Decision::Ask,
                input_schema: json!({"type": "object", "properties": {"text": {"type": "string", "minLength": 1, "maxLength": MAX_WRITE_CHARS, "description": "the text to put on the clipboard"}}, "required": ["text"]}),
                timeout_default_ms: 5_000,
                timeout_max_ms: 10_000,
                annotations: Annotations { read_only: false, destructive: false, idempotent: true, open_world: false },
                untrusted_output: false,
            },
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        match tool {
            "clipboard.read" => Ok(Classification { classes: vec![PermClass::Read], target: "the clipboard".into(), summary: "Read the text on the clipboard".into(), target_allowed: false }),
            "clipboard.write" => {
                let text = text_to_write(args)?;
                Ok(Classification {
                    classes: vec![PermClass::Write],
                    target: "the clipboard".into(),
                    summary: format!("Put {} characters on the clipboard, replacing what is there: \"{}\"", text.chars().count(), preview(text)),
                    target_allowed: false,
                })
            }
            other => Err(invalid(format!("{other} is not a clipboard tool"))),
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let source = self.source.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            match tool.as_str() {
                "clipboard.read" => tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                    let found = source.read_text().map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                    Ok(render_read(found))
                })
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the clipboard task stopped unexpectedly: {e}")))?,
                "clipboard.write" => {
                    let text = text_to_write(&args)?.to_string();
                    tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                        source.write_text(&text).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                        let n = text.chars().count();
                        Ok(ProviderOutput { data: json!({"chars": n}), text: format!("Put {n} characters on the clipboard."), ..Default::default() })
                    })
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the clipboard task stopped unexpectedly: {e}")))?
                }
                other => Err(invalid(format!("{other} is not a clipboard tool"))),
            }
        })
    }
}

/// What a read gives the model: the text, cut at [`MAX_READ_CHARS`] characters, with how much there was. A clipboard with text on it is private; one without is not.
fn render_read(found: Option<String>) -> ProviderOutput {
    let Some(text) = found.filter(|t| !t.is_empty()) else {
        return ProviderOutput { data: json!({"chars": 0, "kind": "none"}), text: "The clipboard holds no text (it may hold a picture, files or nothing).".to_string(), ..Default::default() };
    };
    let total = text.chars().count();
    let shown: String = text.chars().take(MAX_READ_CHARS).collect();
    let left_out = total.saturating_sub(MAX_READ_CHARS);
    let head = if left_out == 0 { format!("Text on the clipboard ({total} characters):") } else { format!("Text on the clipboard ({total} characters, the first {MAX_READ_CHARS} shown):") };
    let tail = if left_out > 0 { format!("\n... {left_out} more characters are not shown.") } else { String::new() };
    ProviderOutput { data: json!({"chars": total, "shown_chars": total - left_out, "left_out": left_out, "kind": "text"}), text: format!("{head}\n{shown}{tail}"), ..Default::default() }.marked_private()
}

#[cfg(windows)]
mod win {
    //! The text of the clipboard of Windows (`CF_UNICODETEXT`), by the plain Win32 calls. The clipboard is open only for the length of one call, and it is
    //! opened again a few times when another program has it (a clipboard manager holds it for a moment after every change).

    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT: u32 = 13;
    /// The most UTF-16 units read from the clipboard (16 million: 32 MB).
    const MAX_UNITS: usize = 16 * 1024 * 1024;

    /// The clipboard, open; closed again when this is dropped.
    struct Open;

    impl Open {
        fn new() -> Result<Self, String> {
            for attempt in 0..10 {
                // SAFETY: OpenClipboard with no window associates the open clipboard with the current task; it has no other precondition.
                if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                    return Ok(Open);
                }
                if attempt < 9 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            Err("the clipboard is in use by another program; try again in a moment".to_string())
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: this program opened the clipboard in `new`.
            unsafe {
                CloseClipboard();
            }
        }
    }

    pub fn read_text() -> Result<Option<String>, String> {
        let _open = Open::new()?;
        // SAFETY: the clipboard is open for the length of this function; the handle belongs to the clipboard and is not freed here; the memory is locked while it is read and
        // unlocked after; reading stops at the first NUL or at the size of the block, whichever comes first.
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                return Ok(None);
            }
            let handle: HANDLE = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                return Ok(None);
            }
            let size_units = GlobalSize(handle) / 2;
            let ptr = GlobalLock(handle) as *const u16;
            if ptr.is_null() {
                return Err("the clipboard text could not be locked for reading".to_string());
            }
            let limit = size_units.min(MAX_UNITS);
            let mut len = 0usize;
            while len < limit && *ptr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            GlobalUnlock(handle);
            Ok(Some(text))
        }
    }

    pub fn write_text(text: &str) -> Result<(), String> {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * 2;
        // SAFETY: the block is made by GlobalAlloc, filled while it is locked, and given to the clipboard, which owns it after a successful SetClipboardData; when anything
        // before that fails it is freed here. The clipboard is open for the length of the calls that use it.
        unsafe {
            let block = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if block.is_null() {
                return Err("there was no memory to put the text on the clipboard".to_string());
            }
            let dest = GlobalLock(block) as *mut u16;
            if dest.is_null() {
                GlobalFree(block);
                return Err("the memory for the clipboard text could not be locked".to_string());
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), dest, units.len());
            GlobalUnlock(block);
            let _open = match Open::new() {
                Ok(open) => open,
                Err(why) => {
                    GlobalFree(block);
                    return Err(why);
                }
            };
            if EmptyClipboard() == 0 {
                GlobalFree(block);
                return Err("the clipboard could not be emptied".to_string());
            }
            if SetClipboardData(CF_UNICODETEXT, block).is_null() {
                GlobalFree(block);
                return Err("the text could not be put on the clipboard".to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A clipboard that is a string in memory, and counts how often it was read and written.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub text: Mutex<Option<String>>,
        pub reads: Mutex<usize>,
        pub writes: Mutex<usize>,
    }

    impl ClipboardSource for Fake {
        fn read_text(&self) -> Result<Option<String>, String> {
            *self.reads.lock().unwrap() += 1;
            Ok(self.text.lock().unwrap().clone())
        }

        fn write_text(&self, text: &str) -> Result<(), String> {
            *self.writes.lock().unwrap() += 1;
            *self.text.lock().unwrap() = Some(text.to_string());
            Ok(())
        }
    }

    fn run(p: &ClipboardProvider, tool: &str, args: Value) -> Result<ProviderOutput, ToolError> {
        let ctx = ExecCtx { request_id: "r".into(), session_id: "s".into(), agent_run_id: "run".into(), cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)), timeout_ms: 5_000, run_holds_outside_text: false, shown: None, progress: None };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(p.call(tool, args, &ctx))
    }

    fn provider(text: Option<&str>) -> (ClipboardProvider, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        *fake.text.lock().unwrap() = text.map(str::to_string);
        (ClipboardProvider::new(fake.clone()), fake)
    }

    #[test]
    fn the_two_tools_are_a_read_that_always_asks_and_a_write_that_asks_and_the_host_accepts_their_manifests() {
        let (p, _) = provider(None);
        let manifests = p.manifests();
        assert_eq!(manifests.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["clipboard.read", "clipboard.write"]);
        for m in &manifests {
            m.check().unwrap_or_else(|e| panic!("the host would refuse {}: {e}", m.name));
        }
        let (read, write) = (&manifests[0], &manifests[1]);
        assert_eq!((read.classes.clone(), read.default_decision, read.untrusted_output, read.annotations.read_only), (vec![PermClass::Read], Decision::AlwaysAsk, true, true));
        assert_eq!((write.classes.clone(), write.default_decision, write.untrusted_output, write.annotations.read_only), (vec![PermClass::Write], Decision::Ask, false, false));
        assert_eq!(write.input_schema["required"], json!(["text"]));
    }

    #[test]
    fn a_write_says_how_much_and_what_it_would_put_with_control_characters_flattened_and_a_long_text_cut() {
        let (p, fake) = provider(None);
        let c = p.classify("clipboard.write", &json!({"text": "line one\nline\ttwo\u{7}"})).unwrap();
        assert_eq!(c.classes, vec![PermClass::Write]);
        assert!(c.summary.contains("18 characters") && c.summary.contains("\"line one line two\""), "{}", c.summary);
        assert!(!c.summary.chars().any(|ch| ch.is_control()), "{}", c.summary);
        assert!(!c.target_allowed, "a write to the clipboard is never vouched for by the provider");
        let long = p.classify("clipboard.write", &json!({"text": "x".repeat(500)})).unwrap();
        assert!(long.summary.contains("500 characters") && long.summary.contains("...") && long.summary.chars().count() < 160, "{}", long.summary);
        assert_eq!(*fake.writes.lock().unwrap(), 0, "classifying changes nothing");
    }

    #[test]
    fn a_write_with_no_text_too_much_text_or_a_tool_that_is_not_ours_is_refused_before_anything_is_touched() {
        let (p, fake) = provider(Some("keep me"));
        for bad in [json!({}), json!({"text": ""}), json!({"text": 5}), json!({"text": "x".repeat(MAX_WRITE_CHARS + 1)})] {
            assert_eq!(p.classify("clipboard.write", &bad).unwrap_err().code, ErrorCode::InvalidArguments, "{bad}");
            assert_eq!(run(&p, "clipboard.write", bad.clone()).unwrap_err().code, ErrorCode::InvalidArguments, "{bad}");
        }
        assert!(p.classify("clipboard.delete", &json!({})).is_err());
        assert_eq!((fake.text.lock().unwrap().clone(), *fake.writes.lock().unwrap()), (Some("keep me".to_string()), 0));
        // The longest text allowed is accepted.
        assert!(p.classify("clipboard.write", &json!({"text": "x".repeat(MAX_WRITE_CHARS)})).is_ok());
    }

    #[test]
    fn a_read_gives_the_text_marks_it_private_and_cuts_a_long_one_saying_how_much_was_left_out() {
        let (p, _) = provider(Some("call Asha at five"));
        let out = run(&p, "clipboard.read", json!({})).unwrap();
        assert!(out.private, "what is on the clipboard is the person's");
        assert!(out.text.contains("call Asha at five") && out.text.contains("17 characters"), "{}", out.text);
        assert_eq!(out.data["kind"], "text");
        let big = "ab".repeat(MAX_READ_CHARS);
        let (p, _) = provider(Some(big.as_str()));
        let out = run(&p, "clipboard.read", json!({})).unwrap();
        assert_eq!(out.data["left_out"], MAX_READ_CHARS, "{}", out.data);
        assert!(out.text.contains("more characters are not shown") && out.text.chars().count() < MAX_READ_CHARS + 200, "{}", out.text.chars().count());
    }

    #[test]
    fn a_clipboard_with_no_text_is_said_so_and_is_not_marked_private() {
        for held in [None, Some("")] {
            let (p, _) = provider(held);
            let out = run(&p, "clipboard.read", json!({})).unwrap();
            assert!(!out.private && out.text.contains("holds no text") && out.data["kind"] == "none", "{held:?}: {}", out.text);
        }
    }

    #[test]
    fn a_write_puts_the_text_on_the_clipboard_and_a_read_gives_it_back() {
        let (p, fake) = provider(Some("before"));
        let out = run(&p, "clipboard.write", json!({"text": "after, with a ünïcode word"})).unwrap();
        assert!(out.text.contains("26 characters"), "{}", out.text);
        assert_eq!(fake.text.lock().unwrap().as_deref(), Some("after, with a ünïcode word"));
        assert!(run(&p, "clipboard.read", json!({})).unwrap().text.contains("ünïcode"));
    }

    #[test]
    fn a_source_that_fails_is_a_backend_error_with_its_reason() {
        struct Broken;
        impl ClipboardSource for Broken {
            fn read_text(&self) -> Result<Option<String>, String> {
                Err("the clipboard is in use by another program".into())
            }
            fn write_text(&self, _text: &str) -> Result<(), String> {
                Err("the text could not be put on the clipboard".into())
            }
        }
        let p = ClipboardProvider::new(Arc::new(Broken));
        let e = run(&p, "clipboard.read", json!({})).unwrap_err();
        assert_eq!(e.code, ErrorCode::BackendError);
        assert!(e.message.contains("in use"), "{}", e.message);
        assert_eq!(run(&p, "clipboard.write", json!({"text": "x"})).unwrap_err().code, ErrorCode::BackendError);
    }
}
