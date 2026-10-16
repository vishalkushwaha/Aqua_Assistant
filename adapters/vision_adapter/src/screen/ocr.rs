//! Reading the text in a picture with the text reader that comes with Windows (`Windows.Media.Ocr`): no model and no download, it runs on the processor in a fraction
//! of a second, and it gives each line and each word with the box it is in. It reads with the languages the person has set up in Windows; a language is added in
//! Settings, Time and language.

use super::find::Reading;
use super::geometry::Shot;

/// Read the text of a picture. The boxes are in pixels of the screen (the picture's own corner is added to them).
#[cfg(windows)]
pub fn read(shot: &Shot) -> Result<Reading, String> {
    // A thread of its own, so that the reading never runs in an apartment that somebody else set up for the thread (the result of a Windows call that is waited
    // for comes back to the thread that is waiting).
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("screen-ocr".into())
            .spawn_scoped(scope, || read_here(shot))
            .map_err(|e| format!("could not start the text reader: {e}"))?
            .join()
            .map_err(|_| "the text reader stopped unexpectedly".to_string())?
    })
}

#[cfg(not(windows))]
pub fn read(_shot: &Shot) -> Result<Reading, String> {
    Err("reading the text of a picture is only available on Windows".into())
}

#[cfg(windows)]
fn why(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {e}")
}

/// A reader for the person's languages, or for English when those have none.
#[cfg(windows)]
fn engine() -> Result<windows::Media::Ocr::OcrEngine, String> {
    use windows::{core::HSTRING, Globalization::Language, Media::Ocr::OcrEngine};
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }
    for tag in ["en-US", "en-GB"] {
        if let Ok(language) = Language::CreateLanguage(&HSTRING::from(tag)) {
            if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&language) {
                return Ok(engine);
            }
        }
    }
    Err("no language that Windows can read text in is installed: add one in Settings, Time and language, Language and region".into())
}

#[cfg(windows)]
fn read_here(shot: &Shot) -> Result<Reading, String> {
    use super::find::{Line, Word};
    use super::geometry::Rect;
    use windows::{
        Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
        Media::Ocr::OcrEngine,
        Storage::Streams::DataWriter,
        Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED},
    };
    // Ignored when the thread has its apartment already: what is needed here works in either.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let engine = engine()?;
    let largest = OcrEngine::MaxImageDimension().unwrap_or(super::geometry::MAX_SIDE);
    if shot.area.w > largest || shot.area.h > largest {
        return Err(format!("the picture is {} by {} pixels and the text reader takes {largest} on a side at most", shot.area.w, shot.area.h));
    }
    let writer = DataWriter::new().map_err(|e| why("could not make a buffer for the picture", e))?;
    writer.WriteBytes(&shot.bgra).map_err(|e| why("could not copy the picture", e))?;
    let buffer = writer.DetachBuffer().map_err(|e| why("could not finish the buffer", e))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, shot.area.w as i32, shot.area.h as i32).map_err(|e| why("Windows would not take the picture", e))?;
    let result = engine.RecognizeAsync(&bitmap).map_err(|e| why("could not start reading", e))?.get().map_err(|e| why("the reading failed", e))?;
    let language = engine.RecognizerLanguage().and_then(|l| l.LanguageTag()).map(|t| t.to_string()).unwrap_or_default();
    let rect_of = |r: &windows::Foundation::Rect| Rect::new(shot.area.x + r.X.round() as i32, shot.area.y + r.Y.round() as i32, r.Width.round().max(1.0) as u32, r.Height.round().max(1.0) as u32);
    let found = result.Lines().map_err(|e| why("could not get the lines", e))?;
    let mut lines = Vec::new();
    for i in 0..found.Size().map_err(|e| why("could not count the lines", e))? {
        let line = found.GetAt(i).map_err(|e| why("could not get a line", e))?;
        let text = line.Text().map_err(|e| why("could not get the text of a line", e))?.to_string();
        let in_line = line.Words().map_err(|e| why("could not get the words of a line", e))?;
        let mut words = Vec::new();
        for j in 0..in_line.Size().map_err(|e| why("could not count the words", e))? {
            let word = in_line.GetAt(j).map_err(|e| why("could not get a word", e))?;
            let at = rect_of(&word.BoundingRect().map_err(|e| why("could not get the place of a word", e))?);
            words.push(Word { text: word.Text().map_err(|e| why("could not get a word", e))?.to_string(), at });
        }
        let Some(at) = words.iter().map(|w| w.at).reduce(|a, b| a.union(&b)) else { continue };
        lines.push(Line { text, at, words });
    }
    Ok(Reading { language, lines })
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::find::find;
    use super::super::geometry::{Rect, Shot};
    use super::read;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, FW_NORMAL, HGDIOBJ, TRANSPARENT,
    };

    /// A white picture with black text drawn in it by Windows itself, as a program would draw it on a window: no file and no screen are needed.
    pub(crate) fn picture_with_text(text: &str, w: u32, h: u32, at: (i32, i32)) -> Shot {
        unsafe {
            let screen = GetDC(null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32);
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!dib.is_null() && !bits.is_null() && !mem.is_null(), "the test could not make a picture");
            let old = SelectObject(mem, dib as HGDIOBJ);
            let brush = CreateSolidBrush(0x00FF_FFFF);
            let all = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            FillRect(mem, &all, brush);
            DeleteObject(brush as HGDIOBJ);
            let face: Vec<u16> = "Arial\0".encode_utf16().collect();
            let font = CreateFontW(-40, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, 0, 0, 0, 0, 0, face.as_ptr());
            let old_font = SelectObject(mem, font as HGDIOBJ);
            SetBkMode(mem, TRANSPARENT as i32);
            SetTextColor(mem, 0);
            let wide: Vec<u16> = text.encode_utf16().collect();
            TextOutW(mem, at.0, at.1, wide.as_ptr(), wide.len() as i32);
            SelectObject(mem, old_font);
            DeleteObject(font as HGDIOBJ);
            let mut bgra = std::slice::from_raw_parts(bits as *const u8, w as usize * h as usize * 4).to_vec();
            SelectObject(mem, old);
            DeleteObject(dib as HGDIOBJ);
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
            bgra.chunks_exact_mut(4).for_each(|px| px[3] = 255);
            Shot::new(Rect::new(300, 100, w, h), bgra).unwrap()
        }
    }

    #[test]
    fn text_drawn_by_windows_is_read_back_with_the_place_of_each_word_in_pixels_of_the_screen() {
        let shot = picture_with_text("Hello Aqua screen reader", 900, 120, (20, 30));
        let reading = read(&shot).unwrap_or_else(|e| panic!("the text reader failed: {e}"));
        assert!(!reading.language.is_empty(), "the language it read with is said");
        let all = reading.lines.iter().map(|l| l.text.to_lowercase()).collect::<Vec<_>>().join(" ");
        assert!(all.contains("hello aqua") && all.contains("reader"), "what was read: {all:?}");
        // The boxes are in pixels of the screen: the picture's corner (300, 100) is in them, and they are inside the picture.
        let area = shot.area;
        for line in &reading.lines {
            assert!(area.intersect(&line.at) == Some(line.at), "{line:?} is not inside {area:?}");
        }
        let hit = find(&reading.lines, "aqua", 3);
        assert_eq!(hit.len(), 1, "{:?}", reading.lines);
        // The text was drawn at (20, 30) in the picture, so "Aqua" is to the right of x = 320 and its middle between y = 130 and y = 190.
        assert!(hit[0].at.x > 300 + 20 && hit[0].center_y > 100 + 30 && hit[0].center_y < 100 + 110, "{hit:?}");
        let first = find(&reading.lines, "hello", 3);
        assert!(first[0].at.x < hit[0].at.x, "Hello is to the left of Aqua");
    }

    #[test]
    fn a_picture_with_no_text_gives_no_lines_and_one_that_is_too_large_is_refused_with_the_limit() {
        let blank = picture_with_text("", 300, 100, (0, 0));
        assert!(read(&blank).unwrap().lines.is_empty());
        let huge = Shot { area: Rect::new(0, 0, 20_000, 10), bgra: vec![255; 20_000 * 10 * 4], hidden: 0 };
        let why = read(&huge).unwrap_err();
        assert!(why.contains("20000") && why.contains("on a side"), "{why}");
    }
}
