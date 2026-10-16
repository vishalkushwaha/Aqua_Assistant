//! What an exit code of a Windows program says (card OA7-01, TM-11).
//!
//! A program that Windows stops (a crash, a missing DLL, a Ctrl+C, a failed fast-fail) exits with an NTSTATUS value. The system hands it over as a signed number, so `0xC0000005` reads
//! `-1073741819` in the result of `term.run`, which says nothing to the model. [`describe`] gives the name and what it usually means; the first line of the result (`exit code -1073741819`) is left as
//! it is, because the code that reads results (`code_verification::exit_code_of`) takes the number from it, and the named line follows it.

use crate::contain::{JobUse, Limits};

/// An NTSTATUS exit code with its name and what it usually means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Named {
    /// The value as an unsigned number (`3221225477`).
    pub value: u32,
    /// `STATUS_ACCESS_VIOLATION`, or `None` for an error status that has no name here.
    pub name: Option<&'static str>,
    pub meaning: &'static str,
}

/// The NTSTATUS values programs most often exit with. (A value is not listed because it is rare; one that is missing is still shown in hex.)
const KNOWN: &[(u32, &str, &str)] = &[
    (0xC000_0005, "STATUS_ACCESS_VIOLATION", "the program read or wrote memory it does not own (a crash)"),
    (0xC000_0135, "STATUS_DLL_NOT_FOUND", "a DLL the program needs was not found (a missing runtime, or a folder that is not on PATH)"),
    (0xC000_013A, "STATUS_CONTROL_C_EXIT", "the program was stopped by Ctrl+C, or its console was closed"),
    (0xC000_0409, "STATUS_STACK_BUFFER_OVERRUN", "the program stopped itself at once (a fast-fail: an abort, a failed memory request in Rust or C++, or a stack buffer overrun)"),
    (0xC000_00FD, "STATUS_STACK_OVERFLOW", "the program used up its stack (endless recursion)"),
    (0xC000_0017, "STATUS_NO_MEMORY", "the system refused the program memory"),
    (0xC000_012D, "STATUS_COMMITMENT_LIMIT", "the program reached a memory limit (the one of the terminal tools, memory_mb in term.json, or the paging file)"),
    (0xC000_0142, "STATUS_DLL_INIT_FAILED", "a DLL the program needs failed to start"),
    (0xC000_0139, "STATUS_ENTRYPOINT_NOT_FOUND", "a DLL the program needs lacks a function the program expects (a wrong version of it)"),
    (0xC000_007B, "STATUS_INVALID_IMAGE_FORMAT", "a 32-bit and a 64-bit file were mixed, or the program file is damaged"),
    (0xC000_0374, "STATUS_HEAP_CORRUPTION", "the program damaged its own heap"),
    (0xC000_001D, "STATUS_ILLEGAL_INSTRUCTION", "the program ran an instruction this processor does not have"),
    (0xC000_0094, "STATUS_INTEGER_DIVIDE_BY_ZERO", "the program divided by zero"),
    (0xC000_0006, "STATUS_IN_PAGE_ERROR", "the system could not read a page of the program from disk"),
    (0x8000_0003, "STATUS_BREAKPOINT", "the program hit a breakpoint or a failed debug assertion"),
];

/// What `code` (the exit code as the system gives it, which is signed) is, when it is an NTSTATUS error: a value that begins `0xC` (an error), or one of the few `0x8` ones that are listed.
/// An ordinary exit code (0, 1, 101, 255...) is `None`.
pub fn describe(code: i32) -> Option<Named> {
    let value = code as u32;
    if let Some((_, name, meaning)) = KNOWN.iter().find(|(v, _, _)| *v == value) {
        return Some(Named { value, name: Some(name), meaning });
    }
    (value >> 28 == 0xC).then_some(Named { value, name: None, meaning: "a Windows error status that has no name in this list; look the value up as an NTSTATUS code" })
}

/// The line that follows `exit code <n>` in a result: `[status] exit code 3221225477 (0xC0000005 STATUS_ACCESS_VIOLATION): the program read or wrote memory it does not own (a crash)`.
pub fn status_line(code: i32) -> Option<String> {
    describe(code).map(|n| match n.name {
        Some(name) => format!("[status] exit code {} (0x{:08X} {name}): {}", n.value, n.value, n.meaning),
        None => format!("[status] exit code {} (0x{:08X}): {}", n.value, n.value, n.meaning),
    })
}

/// The short form for a job: `0xC0000005 STATUS_ACCESS_VIOLATION`.
pub fn short(code: i32) -> Option<String> {
    describe(code).map(|n| match n.name {
        Some(name) => format!("0x{:08X} {name}", n.value),
        None => format!("0x{:08X}", n.value),
    })
}

/// A process this close to its memory limit (in percent of it) is counted as having run into it.
const NEAR_MEMORY_LIMIT_PERCENT: u64 = 90;

/// The note that says a limit of the terminal tools was the cause (or may have been), or `None` when nothing points at one. The memory limit makes a program's request for memory fail
/// (it is not killed), and what a program does then is its own (a Rust program aborts with `0xC0000409`, another prints "out of memory" and exits 1), so the note speaks only of a
/// command that failed and whose biggest process came within a tenth of the limit. A process that the job refused because too many were running is counted by the job itself.
pub fn limit_note(used: JobUse, limits: Limits, failed: bool) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if used.stopped_at_limit > 0 {
        lines.push(format!(
            "[limit] the command asked for more programs than may run at once: {} {} stopped at the limit of {} (max_processes in term.json)",
            used.stopped_at_limit,
            if used.stopped_at_limit == 1 { "program was" } else { "programs were" },
            limits.max_processes
        ));
    }
    let limit_bytes = limits.memory_mb.max(16).saturating_mul(1024 * 1024);
    let near = used.peak_process_memory > 0 && used.peak_process_memory.saturating_mul(100) >= limit_bytes.saturating_mul(NEAR_MEMORY_LIMIT_PERCENT);
    let biggest = used.peak_process_memory / (1024 * 1024);
    if failed && used.refused_memory > 0 {
        // The job said so itself.
        lines.push(format!(
            "[limit] a program of the command was refused memory at the limit of {} MB a program may use (memory_mb in term.json){}",
            limits.memory_mb,
            if used.peak_process_memory > 0 { format!("; the biggest had {biggest} MB") } else { String::new() }
        ));
    } else if failed && near {
        lines.push(format!(
            "[limit] the biggest program of the command had {biggest} MB of the {} MB a program may use (memory_mb in term.json); a command that failed for lack of memory failed at this limit",
            limits.memory_mb
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntstatus_codes_are_named() {
        // The signed number a Windows program's exit code arrives as, and what it is.
        assert_eq!(status_line(-1073741819).unwrap(), "[status] exit code 3221225477 (0xC0000005 STATUS_ACCESS_VIOLATION): the program read or wrote memory it does not own (a crash)");
        for (signed, name) in [
            (0xC000_0005u32 as i32, "STATUS_ACCESS_VIOLATION"),
            (0xC000_0135u32 as i32, "STATUS_DLL_NOT_FOUND"),
            (0xC000_013Au32 as i32, "STATUS_CONTROL_C_EXIT"),
            (0xC000_0409u32 as i32, "STATUS_STACK_BUFFER_OVERRUN"),
            (0xC000_00FDu32 as i32, "STATUS_STACK_OVERFLOW"),
        ] {
            assert_eq!(describe(signed).unwrap().name, Some(name), "{signed:#x}");
        }
        assert_eq!(short(0xC000_0135u32 as i32).unwrap(), "0xC0000135 STATUS_DLL_NOT_FOUND");
        // An error status that is not listed is still shown as the unsigned number and in hex, without a name.
        let unknown = status_line(0xC000_04ACu32 as i32).unwrap();
        assert!(unknown.starts_with("[status] exit code 3221226668 (0xC00004AC):") && !unknown.contains("STATUS_"), "{unknown}");
        // Ordinary exit codes are not NTSTATUS values: nothing is said about them.
        for plain in [0, 1, 2, 3, 101, 127, 255, 1816, -1, i32::MAX] {
            assert!(describe(plain).is_none(), "{plain}");
            assert!(status_line(plain).is_none() && short(plain).is_none(), "{plain}");
        }
    }

    #[test]
    fn a_limit_is_named_only_when_the_job_saw_it() {
        let limits = Limits { memory_mb: 256, max_processes: 4 };
        let mb = 1024 * 1024u64;
        let used = |peak: u64, stopped: u32, refused: u32| JobUse { peak_process_memory: peak, stopped_at_limit: stopped, refused_memory: refused };
        // Nothing seen: no note.
        assert_eq!(limit_note(JobUse::default(), limits, true), None);
        // A command that used little of its memory and failed: no note.
        assert_eq!(limit_note(used(40 * mb, 0, 0), limits, true), None);
        // One close to the limit that succeeded: no note (it did not fail for it).
        assert_eq!(limit_note(used(250 * mb, 0, 0), limits, false), None);
        assert_eq!(limit_note(used(250 * mb, 0, 3), limits, false), None);
        // One close to the limit that failed: the note names the number and the setting.
        let near = limit_note(used(250 * mb, 0, 0), limits, true).unwrap();
        assert!(near.starts_with("[limit]") && near.contains("250 MB of the 256 MB") && near.contains("memory_mb"), "{near}");
        // Exactly 90 percent of 100 MB (94,371,840 bytes) counts, one byte less does not.
        let hundred = Limits { memory_mb: 100, max_processes: 4 };
        assert!(limit_note(used(94_371_840, 0, 0), hundred, true).is_some());
        assert!(limit_note(used(94_371_839, 0, 0), hundred, true).is_none());
        // A program that the job itself says was refused memory, in a command that failed: the note says so, however much the biggest had.
        let refused = limit_note(used(60 * mb, 0, 1), limits, true).unwrap();
        assert!(refused.contains("was refused memory at the limit of 256 MB") && refused.contains("the biggest had 60 MB") && refused.contains("memory_mb"), "{refused}");
        assert!(!limit_note(used(0, 0, 2), limits, true).unwrap().contains("biggest"));
        // Programs the job refused say so, whatever the exit code was.
        let procs = limit_note(used(0, 2, 0), limits, false).unwrap();
        assert!(procs.contains("2 programs were stopped at the limit of 4") && procs.contains("max_processes"), "{procs}");
        assert!(limit_note(used(0, 1, 0), limits, false).unwrap().contains("1 program was stopped"));
        // Both at once are two lines.
        assert_eq!(limit_note(used(255 * mb, 3, 0), limits, true).unwrap().lines().count(), 2);
    }
}
