//! The agent's tools for reading the screen: `screen.ocr(target?, x?, y?, width?, height?)` gives the text on the screen with where each line is, and
//! `screen.find_text(text, target?, ...)` says where on the screen some words are (the middle of them is where a click would go).
//!
//! What is on the screen is the person's own and may be somebody else's words as well (a page, a message, a document), so a reading comes back as **private** (a run that
//! holds it may not send things out without asking) and **untrusted** (the host fences it and the run is marked as having read outside text). Aqua's own windows are made
//! black in the picture before it is read. The picture is kept in memory for the length of the call and is never written to a file. These two tools only look: they click
//! nothing, and clicking on what they find is the desktop tools' business, with their own rules.

use super::capture::{capture, Target};
use super::find::{find, Reading};
use super::geometry::{Rect, Shot, MAX_SIDE};
use aqua_tool_host::{Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider};
use serde_json::{json, Value};
use std::sync::Arc;

/// Lines and characters of one reading that are handed on; the rest is counted and not shown.
const MAX_LINES: usize = 150;
const MAX_TEXT_CHARS: usize = 8_000;
const MAX_MATCHES: usize = 10;

/// Where the pictures and the reading come from: Windows in the real thing, a fixed picture and reading in a test.
pub trait ScreenSource: Send + Sync {
    fn capture(&self, target: Target) -> Result<Shot, String>;
    fn read(&self, shot: &Shot) -> Result<Reading, String>;
}

/// The real source: the screen of this computer and the text reader of Windows.
pub struct WindowsScreen;

impl ScreenSource for WindowsScreen {
    fn capture(&self, target: Target) -> Result<Shot, String> {
        capture(target)
    }

    fn read(&self, shot: &Shot) -> Result<Reading, String> {
        super::ocr::read(shot)
    }
}

pub struct ScreenProvider {
    source: Arc<dyn ScreenSource>,
}

impl ScreenProvider {
    pub fn new(source: Arc<dyn ScreenSource>) -> Self {
        Self { source }
    }

    /// The tools over this computer's own screen.
    pub fn windows() -> Self {
        Self::new(Arc::new(WindowsScreen))
    }
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, message)
}

/// What the arguments ask a picture to be of: all the screens (the default), the window in front, or a rectangle.
fn target_of(args: &Value) -> Result<Target, ToolError> {
    match args.get("target").and_then(|v| v.as_str()).unwrap_or("screen") {
        "screen" => Ok(Target::Screen),
        "window" => Ok(Target::Window),
        "region" => {
            let n = |k: &str| args.get(k).and_then(|v| v.as_i64());
            match (n("x"), n("y"), n("width"), n("height")) {
                (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 && w <= MAX_SIDE as i64 && h <= MAX_SIDE as i64 && x.abs() <= 100_000 && y.abs() <= 100_000 => Ok(Target::Region(Rect::new(x as i32, y as i32, w as u32, h as u32))),
                _ => Err(invalid(format!("a region needs x, y, width and height, in pixels of the screen, with a width and a height from 1 to {MAX_SIDE}"))),
            }
        }
        other => Err(invalid(format!("target is \"screen\", \"window\" or \"region\", not {other:?}"))),
    }
}

fn describe(target: Target) -> (&'static str, String) {
    match target {
        Target::Screen => ("the screen", "Read the text on the screen".to_string()),
        Target::Window => ("the window in front", "Read the text in the window in front".to_string()),
        Target::Region(r) => ("a part of the screen", format!("Read the text in a part of the screen ({} by {} pixels at {}, {})", r.w, r.h, r.x, r.y)),
    }
}

/// The text of a reading for the model: one line each with its box (`[x,y,width,height]` in pixels of the screen), cut at [`MAX_LINES`] lines and
/// [`MAX_TEXT_CHARS`] characters. Also the lines as data, and how many were left out.
fn render(shot_area: Rect, hidden: usize, reading: &Reading) -> (String, Value, usize) {
    let mut text = String::new();
    let mut shown = 0;
    for line in reading.lines.iter().take(MAX_LINES) {
        let one = format!("[{},{},{},{}] {}\n", line.at.x, line.at.y, line.at.w, line.at.h, line.text.split_whitespace().collect::<Vec<_>>().join(" "));
        if text.chars().count() + one.chars().count() > MAX_TEXT_CHARS {
            break;
        }
        text.push_str(&one);
        shown += 1;
    }
    let left_out = reading.lines.len() - shown;
    let head = if reading.lines.is_empty() {
        format!("No text was found in this part of the screen ({} by {} pixels at {}, {}): it may be a picture, a video or text too small or too faint for the reader.", shot_area.w, shot_area.h, shot_area.x, shot_area.y)
    } else {
        format!(
            "Text on the screen ({} by {} pixels at {}, {}; read as {}): {} lines. Each line starts with its box [x,y,width,height] in pixels of the screen.",
            shot_area.w,
            shot_area.h,
            shot_area.x,
            shot_area.y,
            if reading.language.is_empty() { "?" } else { &reading.language },
            reading.lines.len()
        )
    };
    let tail = if left_out > 0 { format!("... {left_out} more lines are not shown: ask for a smaller part of the screen.") } else { String::new() };
    let full = [head, text.trim_end().to_string(), tail].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
    let data = json!({"area": shot_area, "language": reading.language, "total_lines": reading.lines.len(), "lines": reading.lines.iter().take(shown).collect::<Vec<_>>(), "left_out": left_out, "hidden_windows": hidden});
    (full, data, left_out)
}

impl ToolProvider for ScreenProvider {
    fn id(&self) -> &str {
        "screen"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let region = |props: &mut serde_json::Map<String, Value>| {
            props.insert("target".into(), json!({"type": "string", "enum": ["screen", "window", "region"], "default": "screen", "description": "what to read: all the screens, the window in front, or a region"}));
            props.insert("x".into(), json!({"type": "integer", "minimum": -100000, "maximum": 100000, "description": "region only: left edge, in pixels of the screen"}));
            props.insert("y".into(), json!({"type": "integer", "minimum": -100000, "maximum": 100000, "description": "region only: top edge"}));
            props.insert("width".into(), json!({"type": "integer", "minimum": 1, "maximum": MAX_SIDE, "description": "region only"}));
            props.insert("height".into(), json!({"type": "integer", "minimum": 1, "maximum": MAX_SIDE, "description": "region only"}));
        };
        let schema = |extra: Option<(&str, Value)>, required: &[&str]| {
            let mut props = serde_json::Map::new();
            if let Some((k, v)) = extra {
                props.insert(k.into(), v);
            }
            region(&mut props);
            json!({"type": "object", "properties": props, "required": required})
        };
        let m = |name: &str, description: &str, schema: Value| ToolManifest {
            name: name.into(),
            title: name.into(),
            description: description.into(),
            classes: vec![PermClass::Read],
            default_decision: Decision::Auto,
            input_schema: schema,
            timeout_default_ms: 20_000,
            timeout_max_ms: 40_000,
            annotations: Annotations { read_only: true, destructive: false, idempotent: false, open_world: false },
            untrusted_output: true,
        };
        vec![
            m(
                "screen.ocr",
                "Read the text on the screen now (all of it, the window in front, or a region), with where each line is.",
                schema(None, &[]),
            ),
            m(
                "screen.find_text",
                "Say where on the screen some words are: the middle of them, in pixels. It only looks and clicks nothing.",
                schema(Some(("text", json!({"type": "string", "minLength": 1, "maxLength": 200, "description": "the words to find, in the order they are written"}))), &["text"]),
            ),
        ]
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let (target, summary) = describe(target_of(args)?);
        let summary = if tool == "screen.find_text" { format!("Look on {target} for \"{}\"", args.get("text").and_then(|v| v.as_str()).unwrap_or("").chars().take(80).collect::<String>()) } else { summary };
        Ok(Classification { classes: vec![], target: target.to_string(), summary, target_allowed: true })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, _ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let source = self.source.clone();
        let tool = tool.to_string();
        Box::pin(async move {
            let target = target_of(&args)?;
            let wanted = if tool == "screen.find_text" {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if text.trim().is_empty() {
                    return Err(invalid("text is the words to find, and it is empty"));
                }
                Some(text)
            } else {
                None
            };
            tokio::task::spawn_blocking(move || -> Result<ProviderOutput, ToolError> {
                let shot = source.capture(target).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                let reading = source.read(&shot).map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
                let warnings = if shot.hidden > 0 { vec![format!("{} of Aqua's own windows were over this part of the screen and were left out", shot.hidden)] } else { Vec::new() };
                // What is on the screen is the person's, whatever was found in it.
                let output = match wanted {
                    None => {
                        let (text, data, _) = render(shot.area, shot.hidden, &reading);
                        ProviderOutput { data, text, warnings, ..Default::default() }
                    }
                    Some(needle) => {
                        let hits = find(&reading.lines, &needle, MAX_MATCHES);
                        let text = if hits.is_empty() {
                            format!("No line on the screen has the words \"{needle}\". {} lines were read; a different spelling, an icon or text that is too small or faint for the reader would not be found.", reading.lines.len())
                        } else {
                            hits.iter()
                                .enumerate()
                                .map(|(i, h)| format!("[{}] \"{}\" at ({}, {}); box [{},{},{},{}]; in the line \"{}\"", i + 1, h.text, h.center_x, h.center_y, h.at.x, h.at.y, h.at.w, h.at.h, reading.lines[h.line].text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect::<String>()))
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        ProviderOutput { data: json!({"matches": hits, "count": hits.len(), "lines_read": reading.lines.len(), "area": shot.area, "hidden_windows": shot.hidden}), text, warnings, ..Default::default() }
                    }
                };
                Ok(output.marked_private())
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the screen task stopped unexpectedly: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::find::{Line, Word};
    use super::*;
    use std::sync::Mutex;

    /// A screen that is one fixed picture with one fixed reading, and notes what it was asked to take.
    pub(crate) struct Fixed {
        pub reading: Reading,
        pub hidden: usize,
        pub asked: Mutex<Vec<Target>>,
        pub fail_capture: Option<String>,
    }

    impl ScreenSource for Fixed {
        fn capture(&self, target: Target) -> Result<Shot, String> {
            self.asked.lock().unwrap().push(target);
            if let Some(why) = &self.fail_capture {
                return Err(why.clone());
            }
            let area = match target {
                Target::Region(r) => r,
                _ => Rect::new(0, 0, 1920, 1080),
            };
            Ok(Shot { area, bgra: Vec::new(), hidden: self.hidden })
        }

        fn read(&self, _shot: &Shot) -> Result<Reading, String> {
            Ok(self.reading.clone())
        }
    }

    fn line(text: &str, x: i32, y: i32) -> Line {
        let words: Vec<Word> = text.split(' ').enumerate().map(|(i, w)| Word { text: w.to_string(), at: Rect::new(x + i as i32 * 60, y, 55, 18) }).collect();
        let at = words.iter().map(|w| w.at).reduce(|a, b| a.union(&b)).unwrap();
        Line { text: text.to_string(), at, words }
    }

    fn provider(lines: Vec<Line>, hidden: usize) -> (ScreenProvider, Arc<Fixed>) {
        let fixed = Arc::new(Fixed { reading: Reading { language: "en-US".into(), lines }, hidden, asked: Mutex::new(Vec::new()), fail_capture: None });
        (ScreenProvider::new(fixed.clone()), fixed)
    }

    fn run(p: &ScreenProvider, tool: &str, args: Value) -> Result<ProviderOutput, ToolError> {
        let ctx = ExecCtx { request_id: "r".into(), session_id: "s".into(), agent_run_id: "run".into(), cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)), timeout_ms: 5_000, run_holds_outside_text: false, shown: None, progress: None };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(p.call(tool, args, &ctx))
    }

    #[test]
    fn the_two_tools_only_look_and_what_they_return_is_untrusted_and_private() {
        let (p, _) = provider(vec![], 0);
        let manifests = p.manifests();
        assert_eq!(manifests.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["screen.ocr", "screen.find_text"]);
        for m in &manifests {
            assert!(m.untrusted_output && m.annotations.read_only && !m.annotations.destructive, "{}", m.name);
            assert_eq!(m.classes, vec![PermClass::Read]);
            assert_eq!(m.default_decision, Decision::Auto);
            assert!(m.input_schema["properties"]["target"]["enum"].as_array().unwrap().len() == 3);
        }
        assert_eq!(manifests[1].input_schema["required"], json!(["text"]));
        // Whatever was found, the output is marked private; a reading that found nothing is the person's screen too.
        let (p, _) = provider(vec![], 0);
        assert!(run(&p, "screen.ocr", json!({})).unwrap().private);
        assert!(run(&p, "screen.find_text", json!({"text": "Save"})).unwrap().private);
    }

    #[test]
    fn a_reading_gives_each_line_with_its_box_says_what_it_left_out_and_names_the_windows_it_hid() {
        let (p, fixed) = provider(vec![line("File Edit View", 10, 5), line("Save the document", 300, 200)], 2);
        let out = run(&p, "screen.ocr", json!({})).unwrap();
        assert!(out.text.contains("2 lines") && out.text.contains("1920 by 1080") && out.text.contains("en-US"), "{}", out.text);
        assert!(out.text.contains("[10,5,175,18] File Edit View") && out.text.contains("[300,200,175,18] Save the document"), "{}", out.text);
        assert_eq!(out.data["total_lines"], 2);
        assert_eq!(out.data["hidden_windows"], 2);
        assert_eq!(out.data["lines"][1]["text"], "Save the document");
        assert_eq!(out.data["lines"][1]["x"], 300, "the box is flat in the data");
        assert!(out.data["lines"][0].get("words").is_none(), "the words are for finding, not for sending on");
        assert_eq!(out.warnings, vec!["2 of Aqua's own windows were over this part of the screen and were left out".to_string()]);
        assert_eq!(fixed.asked.lock().unwrap().as_slice(), [Target::Screen]);
        // Nothing found says so and says why that can be.
        let (blank, _) = provider(vec![], 0);
        let none = run(&blank, "screen.ocr", json!({"target": "window"})).unwrap();
        assert!(none.text.contains("No text was found") && none.data["total_lines"] == 0, "{}", none.text);
        // A long reading is cut at the lines and the characters, and the number left out is said.
        let many: Vec<Line> = (0..400).map(|i| line(&format!("row {i} of a very long list of words that goes on and on"), 0, i * 20)).collect();
        let (long, _) = provider(many, 0);
        let cut = run(&long, "screen.ocr", json!({})).unwrap();
        assert!(cut.text.chars().count() < super::MAX_TEXT_CHARS + 600, "{}", cut.text.chars().count());
        let left = cut.data["left_out"].as_u64().unwrap();
        assert!(left >= 250 && cut.text.contains(&format!("{left} more lines are not shown")), "{left}: {}", cut.text.lines().last().unwrap());
        assert_eq!(cut.data["lines"].as_array().unwrap().len() as u64 + left, 400);
    }

    #[test]
    fn find_text_gives_the_middle_of_the_words_and_says_when_there_are_none() {
        let (p, _) = provider(vec![line("File Edit View", 10, 5), line("Save the document", 300, 200)], 0);
        let out = run(&p, "screen.find_text", json!({"text": "the document"})).unwrap();
        assert_eq!(out.data["count"], 1);
        // "the" starts at x = 360 and "document" (at 420) ends at 420 + 55 = 475: the box is 360..475 wide, so its middle is 360 + 57 = 417; of 200..218 it is 209.
        assert_eq!((out.data["matches"][0]["center_x"].as_i64(), out.data["matches"][0]["center_y"].as_i64()), (Some(417), Some(209)));
        assert!(out.text.contains("at (417, 209)") && out.text.contains("in the line \"Save the document\""), "{}", out.text);
        let none = run(&p, "screen.find_text", json!({"text": "Cancel"})).unwrap();
        assert!(none.text.contains("No line on the screen has the words \"Cancel\"") && none.data["count"] == 0 && none.data["lines_read"] == 2, "{}", none.text);
        assert!(run(&p, "screen.find_text", json!({"text": "   "})).is_err());
    }

    #[test]
    fn what_to_read_is_checked_before_anything_is_taken_and_a_failure_to_take_it_is_an_error_not_an_empty_reading() {
        let (p, fixed) = provider(vec![], 0);
        for bad in [json!({"target": "desktop"}), json!({"target": "region"}), json!({"target": "region", "x": 0, "y": 0, "width": 0, "height": 10}), json!({"target": "region", "x": 0, "y": 0, "width": 10, "height": 20000})] {
            let e = run(&p, "screen.ocr", bad.clone()).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArguments, "{bad}");
            assert!(p.classify("screen.ocr", &bad).is_err(), "{bad}");
        }
        assert!(fixed.asked.lock().unwrap().is_empty(), "nothing was taken for arguments that were wrong");
        let region = json!({"target": "region", "x": -200, "y": 40, "width": 300, "height": 100});
        run(&p, "screen.ocr", region.clone()).unwrap();
        assert_eq!(fixed.asked.lock().unwrap().as_slice(), [Target::Region(Rect::new(-200, 40, 300, 100))]);
        let c = p.classify("screen.ocr", &region).unwrap();
        assert!(c.summary.contains("300 by 100") && c.target_allowed && c.target == "a part of the screen");
        assert_eq!(p.classify("screen.find_text", &json!({"text": "Save", "target": "window"})).unwrap().summary, "Look on the window in front for \"Save\"");
        // The screen cannot be taken (locked, no display): the error says why.
        let broken = ScreenProvider::new(Arc::new(Fixed { reading: Reading { language: String::new(), lines: vec![] }, hidden: 0, asked: Mutex::new(Vec::new()), fail_capture: Some("Windows would not copy the screen (it may be locked)".into()) }));
        let e = run(&broken, "screen.ocr", json!({})).unwrap_err();
        assert_eq!(e.code, ErrorCode::BackendError);
        assert!(e.message.contains("locked"), "{}", e.message);
    }
}
