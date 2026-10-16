//! What a scheduled run can be checked against before it starts, without asking the model or the person.
//!
//! A run that is certain to fail (the disk it writes to is full) is not tried: it is skipped and said so, and the schedule goes on to its next time. Every check here is
//! plain and certain: nothing that has to be guessed, and a fact that cannot be read passes (a check that cannot tell never blocks a run).

use std::path::Path;

/// What an `Err` from the scheduler's start begins with when the run was not tried (see `Scheduler::could_not_start`).
pub const PREFIX: &str = "preflight: ";

/// The least free space the drive the data lives on must have for a run to start: the run writes its record there at every step, and a database cannot be written to a full disk.
pub const MIN_FREE_BYTES: u64 = 300 * 1024 * 1024;

/// What is known about the machine at the moment of the check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Free bytes on the drive the data is kept on, when that could be read.
    pub free_bytes_on_data_drive: Option<u64>,
}

/// `Err` carries the reason in words (without the prefix).
pub fn check(facts: &Facts) -> Result<(), String> {
    if let Some(free) = facts.free_bytes_on_data_drive {
        if free < MIN_FREE_BYTES {
            return Err(format!("the drive Aqua keeps its data on has only {} MB free (at least {} MB are needed to keep the record of a run)", free / (1024 * 1024), MIN_FREE_BYTES / (1024 * 1024)));
        }
    }
    Ok(())
}

/// The free space on the drive that holds `path`, from a list of drives `(root, kind, total bytes, free bytes)`. `None` when no drive matches.
pub fn free_on_drive_of(path: &Path, drives: &[(String, &'static str, u64, u64)]) -> Option<u64> {
    let letter = |s: &str| {
        let s = s.trim_start_matches(r"\\?\");
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(l), Some(':')) if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase()),
            _ => None,
        }
    };
    let want = letter(&path.to_string_lossy())?;
    drives.iter().find(|(root, ..)| letter(root) == Some(want)).map(|(.., free)| *free)
}

#[cfg(test)]
thread_local! {
    /// A test sets what the check sees: the tests of everything else must not depend on the free space of the machine they run on.
    pub static FREE_FOR_TEST: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// What is known now.
pub fn facts_now() -> Facts {
    #[cfg(test)]
    {
        Facts { free_bytes_on_data_drive: FREE_FOR_TEST.with(|f| f.get()) }
    }
    #[cfg(not(test))]
    {
        let (_, _, drives) = aqua_native_tools::sys::snapshot();
        Facts { free_bytes_on_data_drive: free_on_drive_of(&crate::runtime::data_dir(), &drives) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn a_full_drive_stops_the_run_in_words_and_a_fact_that_could_not_be_read_stops_nothing() {
        assert!(check(&Facts { free_bytes_on_data_drive: Some(5_000 * MB) }).is_ok());
        assert!(check(&Facts { free_bytes_on_data_drive: Some(MIN_FREE_BYTES) }).is_ok(), "the least is enough");
        let why = check(&Facts { free_bytes_on_data_drive: Some(120 * MB) }).unwrap_err();
        assert!(why.contains("only 120 MB free") && why.contains("at least 300 MB"), "{why}");
        assert!(check(&Facts::default()).is_ok(), "unknown passes");
    }

    #[test]
    fn the_drive_of_a_path_is_found_by_its_letter_in_any_case_and_with_the_long_path_prefix() {
        let drives = vec![("C:\\".to_string(), "fixed", 500 * 1024 * MB, 20 * 1024 * MB), ("E:\\".to_string(), "fixed", 80 * 1024 * MB, 3 * 1024 * MB)];
        assert_eq!(free_on_drive_of(&PathBuf::from(r"E:\Personal_AI\Aqua\data"), &drives), Some(3 * 1024 * MB));
        assert_eq!(free_on_drive_of(&PathBuf::from(r"e:\x"), &drives), Some(3 * 1024 * MB));
        assert_eq!(free_on_drive_of(&PathBuf::from(r"\\?\C:\Users\x"), &drives), Some(20 * 1024 * MB));
        assert_eq!(free_on_drive_of(&PathBuf::from(r"Z:\x"), &drives), None, "no such drive");
        assert_eq!(free_on_drive_of(&PathBuf::from("relative/path"), &drives), None, "no letter, no answer");
    }
}
