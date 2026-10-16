//! Voice scratch that a run which was killed left in the temp folder.
//!
//! Everything Aqua writes to the temp folder that holds a recording (the person's voice, as it came and after the noise remover) or the text
//! of a spoken reply is removed when the request that made it ends, on every path that returns or panics. A run that is *killed* (Task
//! Manager, a crash that aborts, a power cut) cannot do that, and the recording stayed there, readable by every program the person runs,
//! until it was cleaned by hand: 14 recordings and a few folders of earlier versions were found in one temp folder. At start-up the ones that
//! are old enough not to belong to a request still in progress are removed.
//!
//! The same goes for the temporary files of the durable-write library (`aqua_tool_host::fsutil`): a state file is written to `.<name>.aqua-tmp-<pid>-<n>` beside
//! it and renamed over it, and a process that is killed in between leaves the temporary file there. [`remove_old_state_temp_files`] removes the old ones from the data
//! folder (and the folders in it, two levels down), by their exact name and only when they are an hour old.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Older than this, the scratch of a request cannot still be in use: a turn is cut off after a few minutes and a recording is under a minute.
const OLDER_THAN: Duration = Duration::from_secs(60 * 60);

/// Is this name one of the files or folders Aqua makes for a request? Only exactly these shapes (a prefix and then only digits), so that
/// nothing else in the temp folder, the person's or another program's, can match.
fn is_scratch(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name.strip_prefix("aqua_whisper_mem_").and_then(|r| r.strip_suffix(".wav")) {
        // process id, then the time in nanoseconds
        return rest.split_once('_').is_some_and(|(pid, stamp)| digits(pid) && digits(stamp));
    }
    if let Some(rest) = name.strip_prefix("aqua_api_wav_").and_then(|r| r.strip_suffix(".wav")) {
        return digits(rest);
    }
    if let Some(rest) = name.strip_prefix("aqua_pipeline_") {
        return digits(rest);
    }
    if let Some(rest) = name.strip_prefix("aqua_kokoro_") {
        return rest.strip_suffix(".wav").or_else(|| rest.strip_suffix(".txt")).is_some_and(digits);
    }
    false
}

/// Remove what is scratch and was last changed `age` or more before `now`, directly inside `dir`. Returns how many were removed.
fn sweep(dir: &Path, age: Duration, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|n| is_scratch(n)) else { continue };
        // `metadata` of an entry does not follow a link: a link is removed as itself, never what it points to.
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta.modified().ok().and_then(|changed| now.duration_since(changed).ok()).is_some_and(|since| since >= age);
        if !old {
            continue;
        }
        let path = dir.join(name);
        let gone = if meta.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if gone.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// At start-up: remove the voice scratch an earlier run that was stopped left in the temp folder. Cheap, and made on a thread of its own by
/// the caller: a temp folder can hold tens of thousands of entries.
pub fn remove_old_voice_scratch() {
    let removed = sweep(&std::env::temp_dir(), OLDER_THAN, SystemTime::now());
    if removed > 0 {
        tracing::info!("removed {removed} recording(s) or reply text(s) that an earlier run which was stopped left in the temp folder");
    }
    let state = remove_old_state_temp_files(&crate::runtime::data_dir(), OLDER_THAN);
    if state > 0 {
        tracing::info!("removed {state} temporary file(s) of state files that an earlier run which was stopped left in the data folder");
    }
}

/// Folders of the data folder that hold no state file of ours and can be big.
const NOT_STATE: &[&str] = &["models", "snapshots", "trash", "cache", "node_modules"];

/// The temporary files of the durable-write library in `data_dir` and the folders in it (two levels down) that are `older_than` or more. Returns how many were removed.
pub fn remove_old_state_temp_files(data_dir: &Path, older_than: Duration) -> usize {
    fn walk(dir: &Path, depth: u32, older_than: Duration) -> usize {
        let mut removed = aqua_tool_host::fsutil::sweep_tmp(dir, older_than);
        if depth == 0 {
            return removed;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return removed };
        for entry in entries.flatten() {
            // A link is not followed (`file_type` of an entry does not follow it).
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            if name.to_str().is_some_and(|n| NOT_STATE.contains(&n)) {
                continue;
            }
            removed += walk(&entry.path(), depth - 1, older_than);
        }
        removed
    }
    walk(data_dir, 2, older_than)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_shapes_of_aquas_own_scratch_match() {
        for yes in [
            "aqua_whisper_mem_14564_1790000000123456789.wav",
            "aqua_api_wav_1790000000123456789.wav",
            "aqua_pipeline_1790000000123456789",
            "aqua_kokoro_1790000000123456789.wav",
            "aqua_kokoro_1790000000123456789.txt",
        ] {
            assert!(is_scratch(yes), "{yes}");
        }
        for no in [
            "aqua_whisper_mem_.wav",
            "aqua_whisper_mem_14564.wav",
            "aqua_whisper_mem_14564_17900x.wav",
            "aqua_api_wav_.wav",
            "aqua_api_wav_12.mp3",
            "aqua_api_wav_12.wav.bak",
            "aqua_pipeline_",
            "aqua_pipeline_social_12",
            "aqua_pipeline_12_extra",
            "aqua_kokoro_12.png",
            "aqua_kokoro_",
            "aqua_audit_rotations_12",
            "aqua_core.log",
            "my_aqua_api_wav_12.wav",
            "notes.txt",
            "",
        ] {
            assert!(!is_scratch(no), "{no:?}");
        }
    }

    #[test]
    fn the_temporary_files_a_killed_writer_left_in_the_data_folder_are_removed_when_old_and_nothing_else_is() {
        let dir = std::env::temp_dir().join(format!("aqua_sweep_state_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::create_dir_all(dir.join("learning/deeper/deepest")).unwrap();
        std::fs::create_dir_all(dir.join("models")).unwrap();
        let orphan = |p: std::path::PathBuf| {
            std::fs::write(&p, b"half").unwrap();
            p
        };
        let a = orphan(dir.join(".door_shadow.json.aqua-tmp-4242-1"));
        let b = orphan(dir.join("tools").join(".autonomy.json.aqua-tmp-4242-2"));
        let c = orphan(dir.join("learning/deeper").join(".x.json.aqua-tmp-1-3"));
        let too_deep = orphan(dir.join("learning/deeper/deepest").join(".y.json.aqua-tmp-1-4"));
        let in_models = orphan(dir.join("models").join(".m.json.aqua-tmp-1-5"));
        let state = dir.join("tools").join("autonomy.json");
        std::fs::write(&state, b"{}").unwrap();
        let lookalike = orphan(dir.join("tools").join("notes.aqua-tmp-notes"));
        // Just written: a writer may be in the middle of it.
        assert_eq!(remove_old_state_temp_files(&dir, Duration::from_secs(3600)), 0);
        assert!(a.exists() && b.exists());
        // Old enough: the three within two levels go; deeper, the models folder, the real state file and the look-alike stay.
        assert_eq!(remove_old_state_temp_files(&dir, Duration::ZERO), 3);
        assert!(!a.exists() && !b.exists() && !c.exists());
        assert!(too_deep.exists() && in_models.exists() && state.exists() && lookalike.exists());
        assert_eq!(remove_old_state_temp_files(&dir.join("missing"), Duration::ZERO), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_scratch_is_removed_fresh_scratch_and_everything_else_stay() {
        let dir = std::env::temp_dir().join(format!("aqua_sweep_check_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scratch_file = dir.join("aqua_api_wav_1790000000123456789.wav");
        let scratch_dir = dir.join("aqua_pipeline_1790000000123456789");
        let other_file = dir.join("holiday_photos.zip");
        let other_aqua = dir.join("aqua_core.log");
        std::fs::write(&scratch_file, b"voice").unwrap();
        std::fs::create_dir_all(scratch_dir.join("df")).unwrap();
        std::fs::write(scratch_dir.join("df").join("x.wav"), b"voice").unwrap();
        std::fs::write(&other_file, b"keep").unwrap();
        std::fs::write(&other_aqua, b"keep").unwrap();

        // Just made: a request may be using it. Nothing goes.
        assert_eq!(sweep(&dir, OLDER_THAN, SystemTime::now()), 0);
        assert!(scratch_file.exists() && scratch_dir.exists());

        // Two hours later the same files are old: the scratch goes (the folder with what it holds), the rest stays.
        let later = SystemTime::now() + Duration::from_secs(2 * 60 * 60);
        assert_eq!(sweep(&dir, OLDER_THAN, later), 2);
        assert!(!scratch_file.exists() && !scratch_dir.exists());
        assert!(other_file.exists() && other_aqua.exists());
        assert_eq!(sweep(&dir, OLDER_THAN, later), 0, "nothing more to remove");
        assert_eq!(sweep(&dir.join("not_here"), OLDER_THAN, later), 0, "a folder that is not there is not an error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
