//! The few Windows facts the index needs: which volumes exist and where they are mounted, whether the machine is busy,
//! and the CPU time the indexer thread has used. Everything has a plain fallback so the crate builds elsewhere.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolInfo {
    /// Stable identity of the volume (survives a drive letter change).
    pub guid: String,
    /// Where it is mounted now (`C:\`), or None when it is not connected.
    pub mount: Option<PathBuf>,
    pub fixed: bool,
}

/// The volumes known right now. Filled from Windows, or by a test.
#[derive(Debug, Clone, Default)]
pub struct VolumeMap {
    pub list: Vec<VolInfo>,
}

impl VolumeMap {
    pub fn single(guid: &str, mount: &Path) -> Self {
        Self { list: vec![VolInfo { guid: guid.to_string(), mount: Some(mount.to_path_buf()), fixed: true }] }
    }

    /// The mount of an online volume.
    pub fn mount_of(&self, guid: &str) -> Option<PathBuf> {
        self.list.iter().find(|v| v.guid == guid).and_then(|v| v.mount.clone())
    }

    pub fn is_online(&self, guid: &str) -> bool {
        self.mount_of(guid).is_some()
    }

    /// The volume a path is on, and the part of the path below the volume's mount. The longest mount wins.
    pub fn locate(&self, path: &Path) -> Option<(&VolInfo, PathBuf)> {
        let lower = path.to_string_lossy().to_lowercase();
        let mut best: Option<(&VolInfo, usize)> = None;
        for v in &self.list {
            let Some(m) = &v.mount else { continue };
            let ml = m.to_string_lossy().to_lowercase();
            let ml = ml.trim_end_matches('\\');
            let ok = lower == ml || lower.starts_with(&format!("{ml}\\"));
            if ok && best.is_none_or(|(_, l)| ml.len() > l) {
                best = Some((v, ml.len()));
            }
        }
        best.map(|(v, len)| {
            let rest = path.to_string_lossy()[len..].trim_start_matches('\\').to_string();
            (v, PathBuf::from(rest))
        })
    }
}

/// A path that Windows file calls accept beyond 260 characters.
pub fn long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if cfg!(windows) && s.len() >= 240 && !s.starts_with(r"\\?\") && !s.starts_with(r"\\") {
        PathBuf::from(format!(r"\\?\{s}"))
    } else {
        p.to_path_buf()
    }
}

/// Is this a path the index may use at all: a local drive path, not a network or device path.
pub fn is_local_drive_path(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && !p.starts_with(r"\\")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeNameForVolumeMountPointW};
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    use windows_sys::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn snapshot_volumes() -> VolumeMap {
        let mut list = Vec::new();
        // SAFETY: plain Win32 queries with valid NUL-terminated strings and a correctly sized buffer.
        unsafe {
            let mask = GetLogicalDrives();
            for i in 0..26u32 {
                if mask & (1 << i) == 0 {
                    continue;
                }
                let root = format!("{}:\\", (b'A' + i as u8) as char);
                let kind = GetDriveTypeW(wide(&root).as_ptr());
                // 3 = fixed disk. Removable, network and optical drives are not indexed on their own.
                if kind != 3 {
                    continue;
                }
                let mut buf = [0u16; 64];
                let ok = GetVolumeNameForVolumeMountPointW(wide(&root).as_ptr(), buf.as_mut_ptr(), buf.len() as u32);
                let guid = if ok != 0 {
                    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                    String::from_utf16_lossy(&buf[..n])
                } else {
                    format!("letter:{root}")
                };
                list.push(VolInfo { guid, mount: Some(PathBuf::from(root)), fixed: true });
            }
        }
        VolumeMap { list }
    }

    fn power() -> Option<SYSTEM_POWER_STATUS> {
        // SAFETY: the struct is plain data and is filled by the call.
        unsafe {
            let mut s: SYSTEM_POWER_STATUS = std::mem::zeroed();
            (GetSystemPowerStatus(&mut s) != 0).then_some(s)
        }
    }

    pub fn on_battery() -> bool {
        power().is_some_and(|s| s.ACLineStatus == 0)
    }

    pub fn battery_saver() -> bool {
        power().is_some_and(|s| s.SystemStatusFlag == 1)
    }

    /// A full-screen game or app, or presentation mode: not the time to scan.
    pub fn user_busy_fullscreen() -> bool {
        // SAFETY: out parameter is a plain integer.
        unsafe {
            let mut state = 0;
            if SHQueryUserNotificationState(&mut state) != 0 {
                return false;
            }
            state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
        }
    }

    /// CPU time (kernel + user) this thread has used, in milliseconds.
    pub fn thread_cpu_ms() -> u64 {
        // SAFETY: the FILETIMEs are plain data filled by the call.
        unsafe {
            let (mut c, mut e, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = std::mem::zeroed();
            if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) == 0 {
                return 0;
            }
            let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
            (ft(k) + ft(u)) / 10_000
        }
    }

    /// Low CPU and low disk priority for the calling thread: it only gets what nobody else wants.
    pub fn set_background_priority() {
        // SAFETY: affects only the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN as i32);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn snapshot_volumes() -> VolumeMap {
        VolumeMap::single("root", Path::new("/"))
    }
    pub fn on_battery() -> bool {
        false
    }
    pub fn battery_saver() -> bool {
        false
    }
    pub fn user_busy_fullscreen() -> bool {
        false
    }
    pub fn thread_cpu_ms() -> u64 {
        0
    }
    pub fn set_background_priority() {}
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_located_on_the_volume_with_the_longest_mount() {
        let m = VolumeMap {
            list: vec![
                VolInfo { guid: "a".into(), mount: Some(PathBuf::from("C:\\")), fixed: true },
                VolInfo { guid: "b".into(), mount: Some(PathBuf::from("D:\\")), fixed: true },
                VolInfo { guid: "off".into(), mount: None, fixed: true },
            ],
        };
        let (v, rest) = m.locate(Path::new("D:\\Games\\NFS")).unwrap();
        assert_eq!((v.guid.as_str(), rest), ("b", PathBuf::from("Games\\NFS")));
        assert!(m.locate(Path::new("E:\\x")).is_none());
        let (v, rest) = m.locate(Path::new("c:\\")).unwrap();
        assert_eq!((v.guid.as_str(), rest), ("a", PathBuf::new()));
        assert!(m.mount_of("off").is_none() && !m.is_online("off") && m.is_online("a"));
    }

    #[test]
    fn only_local_drive_paths_are_accepted() {
        assert!(is_local_drive_path("D:\\Games") && is_local_drive_path("c:/x"));
        assert!(!is_local_drive_path(r"\\server\share") && !is_local_drive_path(r"\\?\C:\x") && !is_local_drive_path("relative"));
    }
}
