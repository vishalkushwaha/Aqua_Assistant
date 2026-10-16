//! How many cores this computer really has. Work that streams through memory (the language model, speech recognition) is limited
//! by memory bandwidth and by the cores' shared caches, and the second thread of a core (hyper-threading) adds little to it. On
//! this laptop (4 cores, 8 threads) the language model wrote 4.7 tokens a second with 4 threads and 4.0 with 8, and read a prompt
//! only 6 % faster with 8. So the number of threads follows the cores, and the other logical processors stay free for the window,
//! the microphone and the helpers.

use std::sync::OnceLock;

/// Logical processors: what Windows calls "processors", hyper-threads included.
pub fn logical_processors() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).max(1)
}

/// Physical cores, at least 1. When the system cannot say, half of the logical processors (rounded up).
pub fn physical_cores() -> usize {
    static CORES: OnceLock<usize> = OnceLock::new();
    *CORES.get_or_init(|| detect().filter(|n| *n >= 1).unwrap_or_else(|| logical_processors().div_ceil(2)).min(logical_processors()))
}

#[cfg(windows)]
fn detect() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX};
    // SAFETY: the first call only asks how big the answer is; the second fills a buffer of that size, and the records in it
    // (one per physical core) are walked by the size each record states.
    unsafe {
        let mut len = 0u32;
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        if GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX, &mut len) == 0 {
            return None;
        }
        let (mut cores, mut offset) = (0usize, 0usize);
        while offset + std::mem::size_of::<u32>() * 2 <= len as usize {
            let record = &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            if record.Size == 0 {
                break;
            }
            cores += 1;
            offset += record.Size as usize;
        }
        Some(cores)
    }
}

#[cfg(not(windows))]
fn detect() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_count_is_sensible() {
        let (cores, logical) = (physical_cores(), logical_processors());
        assert!(cores >= 1 && cores <= logical, "{cores} cores, {logical} logical processors");
        // On a machine with hyper-threading (as this laptop: 4 cores, 8 threads) there are fewer cores than threads.
        eprintln!("{cores} physical cores, {logical} logical processors");
    }
}
