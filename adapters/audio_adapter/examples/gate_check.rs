//! `cargo run --release -p aqua-audio-adapter --example gate_check -- clip.wav folder ...` prints what the speech gate makes of each
//! clip (a folder means every .wav in it).
use std::path::{Path, PathBuf};

fn clips(arg: &str) -> Vec<PathBuf> {
    let p = Path::new(arg);
    if p.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(p).map(|d| d.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "wav")).collect()).unwrap_or_default();
        v.sort();
        v
    } else {
        vec![p.to_path_buf()]
    }
}

fn main() {
    for arg in std::env::args().skip(1) {
        for path in clips(&arg) {
            let mut r = match hound::WavReader::open(&path) {
                Ok(r) => r,
                Err(e) => {
                    println!("{}: {e}", path.display());
                    continue;
                }
            };
            let spec = r.spec();
            let samples: Vec<f32> = r.samples::<i16>().filter_map(Result::ok).map(|s| s as f32 / 32768.0).collect();
            let mono: Vec<f32> = samples.chunks(spec.channels as usize).map(|c| c[0]).collect();
            let t = std::time::Instant::now();
            let rep = aqua_audio_adapter::speech_gate::analyze(&mono, if std::env::var_os("STRICT").is_some() { aqua_audio_adapter::speech_gate::Strictness::Strict } else { aqua_audio_adapter::speech_gate::Strictness::Lenient });
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            println!("{:<34} {:?} voiced {} ms, longest {} ms, active {} ms ({:?})", name, rep.verdict, rep.voiced_ms, rep.longest_run_ms, rep.active_ms, t.elapsed());
        }
    }
}
