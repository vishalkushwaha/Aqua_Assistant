use super::image::card::CardProvider;
use super::image::diffusion::DiffusionProvider;
use super::image::*;
use super::image_exec::ImageNodeExecutor;
use super::model::*;
use super::run::exec::*;
use super::supervisor::{classify, ErrorClass};
use base64::Engine;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("aqua_img_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn req(title: &str, w: u32, h: u32) -> ImageRequest {
    ImageRequest { prompt: String::new(), title: title.into(), subtitle: String::new(), width: w, height: h, style: "ocean".into(), seed: None }
}

/// The system's font, or None on a machine that has none (then the honest error is what is tested).
fn card() -> Option<CardProvider> {
    let c = CardProvider::system();
    c.ready().ok().map(|_| c)
}

fn decode(path: &std::path::Path) -> image::RgbImage {
    image::open(path).unwrap().to_rgb8()
}

fn near(p: &image::Rgb<u8>, c: [u8; 3]) -> bool {
    p.0.iter().zip(c).all(|(a, b)| (*a as i32 - b as i32).abs() < 40)
}

// ---- the card --------------------------------------------------------------------------------------------------------------------

#[test]
fn a_card_is_a_real_png_of_the_size_asked_with_the_headline_drawn_on_it() {
    let Some(c) = card() else { return };
    let d = dir("card");
    let r = c.generate(&req("Five ways agents save time", 1200, 627), &d, "run1_cover").unwrap();
    assert_eq!((r.width, r.height, r.provider), (1200, 627, "card"));
    assert!(r.path.ends_with("run1_cover.png"));
    let bytes = std::fs::read(&r.path).unwrap();
    assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], "a real PNG signature");
    assert!(bytes.len() > 5_000, "a drawn image, not a header: {} bytes", bytes.len());
    let img = decode(&r.path);
    assert_eq!(img.dimensions(), (1200, 627));
    let text_px = img.pixels().filter(|p| near(p, [240, 249, 255])).count();
    assert!(text_px > 2_000, "the headline is there: {text_px} text pixels");
    assert!(r.alt_text.contains("Five ways agents save time"), "{}", r.alt_text);
    // The background really is a gradient.
    assert_ne!(img.get_pixel(2, 2), img.get_pixel(1197, 624));
}

#[test]
fn a_card_is_deterministic_and_the_style_and_subtitle_change_it() {
    let Some(c) = card() else { return };
    let d = dir("card_det");
    let a = c.generate(&req("Hello", 800, 450), &d, "a").unwrap();
    let b = c.generate(&req("Hello", 800, 450), &d, "b").unwrap();
    assert_eq!(std::fs::read(&a.path).unwrap(), std::fs::read(&b.path).unwrap(), "the same request draws the same bytes");
    let mut sunset = req("Hello", 800, 450);
    sunset.style = "sunset".into();
    let s = c.generate(&sunset, &d, "s").unwrap();
    assert_ne!(decode(&a.path).get_pixel(5, 5), decode(&s.path).get_pixel(5, 5), "another colour scheme");
    let mut sub = req("Hello", 800, 450);
    sub.subtitle = "A quiet week in review".into();
    let t = c.generate(&sub, &d, "t").unwrap();
    assert!(t.alt_text.contains("A quiet week in review"));
    assert_ne!(std::fs::read(&a.path).unwrap(), std::fs::read(&t.path).unwrap());
}

#[test]
fn a_long_headline_wraps_and_shrinks_to_fit_and_never_runs_off_the_edges() {
    let Some(c) = card() else { return };
    let d = dir("card_wrap");
    let long = "Autonomous workflow agents that schedule themselves, recover from errors and tell you honestly what happened, every single morning";
    let r = c.generate(&req(long, 600, 400), &d, "long").unwrap();
    let img = decode(&r.path);
    let margin = (400f32 * 0.09).max(12.0) as u32; // the card's margin is from the smaller side
    let text_in = |x0: u32, x1: u32| (x0..x1).any(|x| (0..img.height()).any(|y| near(img.get_pixel(x, y), [240, 249, 255])));
    assert!(text_in(margin, 600 - margin), "there is text");
    assert!(!text_in(0, margin / 2) && !text_in(600 - margin / 2, 600), "nothing is drawn in the outer margin");
    // A word wider than the card is broken rather than cut off.
    let r = c.generate(&req("Supercalifragilisticexpialidocious_and_more_letters", 300, 300), &d, "word").unwrap();
    let img = decode(&r.path);
    assert!(!(290..300).any(|x| (0..300).any(|y| near(img.get_pixel(x, y), [240, 249, 255]))), "not cut off at the right edge");
}

#[test]
fn nonsense_requests_and_a_missing_font_are_refused_in_words() {
    let d = dir("card_bad");
    let missing = CardProvider::with_candidates(vec![PathBuf::from("/no/such/font.ttf")]);
    let e = missing.generate(&req("Hi", 400, 300), &d, "x").unwrap_err();
    assert!(matches!(&e, ImageError::NotSetUp(m) if m.contains("needs a font") && m.contains("AQUA_FONT")), "{e:?}");
    assert!(!d.exists(), "nothing was written");
    if let Some(c) = card() {
        assert!(matches!(c.generate(&req("", 400, 300), &d, "x"), Err(ImageError::Invalid(m)) if m.contains("needs a title")));
        assert!(matches!(c.generate(&req("Hi", 10, 300), &d, "x"), Err(ImageError::Invalid(m)) if m.contains("outside")));
        assert!(matches!(c.generate(&req("Hi", 400, 9000), &d, "x"), Err(ImageError::Invalid(_))));
    }
    assert_eq!(parse_size("1200x627").unwrap(), (1200, 627));
    assert_eq!(parse_size("1080 × 1350").unwrap(), (1080, 1350));
    assert!(parse_size("big").is_err() && parse_size("0x0").is_err());
    assert_eq!(safe_stem("../../etc/passwd"), "______etc_passwd");
}

// ---- the local image generator ------------------------------------------------------------------------------------------------

/// A stand-in for a Stable Diffusion service. `answer` is the status and body it sends back.
fn fake_service(status: u16, body: String, seen: Arc<Mutex<Vec<(String, Value)>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (mut head_end, mut want) = (None, 0usize);
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if head_end.is_none() {
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = Some(p + 4);
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        want = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    }
                }
                if let Some(h) = head_end {
                    if buf.len() >= h + want {
                        break;
                    }
                }
            }
            let h = head_end.unwrap_or(buf.len());
            let line = String::from_utf8_lossy(&buf[..h]).lines().next().unwrap_or("").to_string();
            let json: Value = serde_json::from_slice(&buf[h..]).unwrap_or(Value::Null);
            seen.lock().unwrap().push((line, json));
            let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = s.write_all(resp.as_bytes());
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn png_b64(w: u32, h: u32) -> String {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 4) as u8, (y * 4) as u8, 120]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
}

fn prompt(p: &str, w: u32, h: u32) -> ImageRequest {
    ImageRequest { prompt: p.into(), title: String::new(), subtitle: String::new(), width: w, height: h, style: String::new(), seed: Some(7) }
}

#[test]
fn a_prompt_is_sent_to_the_local_service_and_the_answer_is_checked_and_saved_as_a_real_image() {
    let seen = Arc::new(Mutex::new(vec![]));
    let url = fake_service(200, json!({ "images": [png_b64(64, 48)] }).to_string(), seen.clone());
    let p = DiffusionProvider::new(&url, Duration::from_secs(10));
    assert!(p.ready().is_ok());
    let d = dir("diff");
    let r = p.generate(&prompt("a lighthouse at dawn", 512, 512), &d, "run2_pic").unwrap();
    let (line, body) = seen.lock().unwrap()[0].clone();
    assert!(line.starts_with("POST /sdapi/v1/txt2img"), "{line}");
    assert_eq!((body["prompt"].as_str(), body["width"].as_u64(), body["height"].as_u64(), body["seed"].as_u64()), (Some("a lighthouse at dawn"), Some(512), Some(512), Some(7)));
    assert_eq!((r.width, r.height, r.provider), (64, 48, "diffusion"), "the size is what the image really is");
    assert_eq!(decode(&r.path).dimensions(), (64, 48));
    assert!(r.alt_text.contains("a lighthouse at dawn"));
}

#[test]
fn a_service_that_fails_or_lies_never_produces_a_file_and_is_retried_only_when_that_may_help() {
    let d = dir("diff_bad");
    let seen = Arc::new(Mutex::new(vec![]));
    let busy = DiffusionProvider::new(&fake_service(503, "{}".into(), seen.clone()), Duration::from_secs(10));
    assert!(matches!(busy.generate(&prompt("x", 512, 512), &d, "a"), Err(ImageError::Failed { retryable: true, .. })), "a busy service may work next time");
    let refused = DiffusionProvider::new(&fake_service(422, "{}".into(), seen.clone()), Duration::from_secs(10));
    assert!(matches!(refused.generate(&prompt("x", 512, 512), &d, "a"), Err(ImageError::Failed { retryable: false, .. })));
    for body in ["not json at all".to_string(), json!({ "images": [] }).to_string(), json!({ "images": ["@@@not base64@@@"] }).to_string(), json!({ "images": [base64::engine::general_purpose::STANDARD.encode("just some text")] }).to_string()] {
        let p = DiffusionProvider::new(&fake_service(200, body.clone(), seen.clone()), Duration::from_secs(10));
        let e = p.generate(&prompt("x", 512, 512), &d, "a").unwrap_err();
        assert!(matches!(e, ImageError::Failed { retryable: false, .. }), "{body}: {e:?}");
    }
    assert!(!d.join("a.png").exists(), "nothing was saved from any of those");
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let down = DiffusionProvider::new(&format!("http://127.0.0.1:{port}"), Duration::from_secs(3));
    let e = down.generate(&prompt("x", 512, 512), &d, "a").unwrap_err();
    assert!(matches!(&e, ImageError::Failed { retryable: true, message } if message.contains("could not connect")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Network);
    assert!(matches!(DiffusionProvider::new(&fake_service(200, "{}".into(), seen), Duration::from_secs(3)).generate(&prompt("  ", 512, 512), &d, "a"), Err(ImageError::Invalid(_))));
}

#[test]
fn the_local_service_must_really_be_local_and_when_it_is_missing_the_step_says_how_to_set_it_up() {
    let d = dir("diff_scope");
    for bad in ["http://example.com", "http://192.168.1.20:7860", "https://127.0.0.1:7860", "ftp://127.0.0.1"] {
        let e = DiffusionProvider::new(bad, Duration::from_secs(1)).generate(&prompt("x", 512, 512), &d, "a").unwrap_err();
        assert!(matches!(e, ImageError::NotSetUp(_)), "{bad}: {e:?}");
    }
    let none = DiffusionProvider::new("", Duration::from_secs(1));
    let e = none.ready().unwrap_err();
    assert!(matches!(&e, ImageError::NotSetUp(m) if m.contains("AQUA_IMAGE_ENDPOINT") && m.contains("card")), "{e:?}");
    assert!(!d.exists());
}

// ---- the step ----------------------------------------------------------------------------------------------------------------------

fn step(config: Value) -> Result<NodeResult, NodeError> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let d = dir(&format!("step{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
    let exec = ImageNodeExecutor::new(Arc::new(provider), d);
    let node: Node = serde_json::from_value(json!({ "id": "cover", "kind": "image_generation", "name": "Cover", "config": config })).unwrap();
    let cancel = AtomicBool::new(false);
    exec.execute(&NodeCtx { run_id: 9, node: &node, config: node.config.clone(), cancel: &cancel, timezone: "" })
}

#[test]
fn the_step_makes_the_file_and_hands_on_its_path_and_a_description() {
    if card().is_none() {
        return;
    }
    let r = step(json!({ "provider": "card", "title": "Launch day", "subtitle": "We are live", "style": "violet", "size": "1080x1080" })).unwrap();
    let path = r.outputs["image"].as_str().unwrap();
    assert!(path.ends_with("run9_cover.png") && std::path::Path::new(path).exists(), "{path}");
    assert_eq!(decode(std::path::Path::new(path)).dimensions(), (1080, 1080));
    assert!(r.outputs["alt_text"].as_str().unwrap().contains("Launch day"));
    let own = step(json!({ "provider": "card", "title": "Launch day", "alt_text": "The word Launch in white on purple" })).unwrap();
    assert_eq!(own.outputs["alt_text"], "The word Launch in white on purple", "a description the person wrote is kept");
}

#[test]
fn the_step_reports_a_missing_generator_and_bad_settings_as_settings_not_as_outages() {
    // With nothing configured, the default provider is not there, and the step says how to fix it.
    std::env::remove_var("AQUA_IMAGE_ENDPOINT");
    let e = step(json!({ "prompt": "a robot" })).unwrap_err();
    assert!(matches!(&e, NodeError::Fatal(m) if m.contains("no image generator is set up")), "{e:?}");
    assert_eq!(classify(e.message()), ErrorClass::Unknown, "not retried, and not mistaken for a network problem");
    let old = step(json!({ "provider": "local", "prompt": "a robot" })).unwrap_err();
    assert!(matches!(&old, NodeError::Fatal(m) if m.contains("no image generator is set up")), "the old name still means diffusion: {old:?}");
    for (cfg, needle) in [(json!({ "provider": "midjourney", "prompt": "x" }), "not an image provider"), (json!({ "provider": "card", "title": "x", "size": "huge" }), "is not a size"), (json!({ "provider": "card", "title": "x", "size": "9000x9000" }), "outside")] {
        let e = step(cfg.clone()).unwrap_err();
        assert!(matches!(&e, NodeError::Fatal(m) if m.contains(needle)), "{cfg}: {e:?}");
        assert_eq!(classify(e.message()), ErrorClass::InvalidInput, "{cfg}");
    }
}
