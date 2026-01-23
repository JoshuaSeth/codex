use std::path::PathBuf;

/// Best-effort lookup of the current process's parent working directory.
///
/// This is used by TUIs to follow the user's shell `cwd` after SIGTSTP resume
/// (e.g., start Codex, Ctrl+Z, `cd` elsewhere, then `fg`).
pub fn parent_cwd() -> Option<PathBuf> {
    let ppid = unsafe { libc::getppid() };
    if ppid <= 0 {
        return None;
    }
    pid_cwd(ppid)
}

#[cfg(target_os = "linux")]
fn pid_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    let link = PathBuf::from(format!("/proc/{pid}/cwd"));
    std::fs::read_link(link).ok()
}

#[cfg(target_os = "macos")]
fn pid_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // From macOS SDK `sys/proc_info.h`:
    //   #define PROC_PIDVNODEPATHINFO      9
    //   #define PROC_PIDVNODEPATHINFO_SIZE (sizeof(struct proc_vnodepathinfo))
    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;

    // From macOS SDK `sys/param.h`:
    //   #define MAXPATHLEN 1024
    const MAXPATHLEN: usize = 1024;

    #[repr(C)]
    struct VinfoStat {
        vst_dev: u32,
        vst_mode: u16,
        vst_nlink: u16,
        vst_ino: u64,
        vst_uid: libc::uid_t,
        vst_gid: libc::gid_t,
        vst_atime: i64,
        vst_atimensec: i64,
        vst_mtime: i64,
        vst_mtimensec: i64,
        vst_ctime: i64,
        vst_ctimensec: i64,
        vst_birthtime: i64,
        vst_birthtimensec: i64,
        vst_size: libc::off_t,
        vst_blocks: i64,
        vst_blksize: i32,
        vst_flags: u32,
        vst_gen: u32,
        vst_rdev: u32,
        vst_qspare: [i64; 2],
    }

    #[repr(C)]
    struct VnodeInfo {
        vi_stat: VinfoStat,
        vi_type: libc::c_int,
        vi_pad: libc::c_int,
        vi_fsid: libc::fsid_t,
    }

    #[repr(C)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [libc::c_char; MAXPATHLEN],
    }

    #[repr(C)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        pvi_rdir: VnodeInfoPath,
    }

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
    let bytes = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int,
        )
    };
    if bytes <= 0 {
        return None;
    }

    let cstr = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr()) };
    let path = cstr.to_bytes();
    (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn pid_cwd(_pid: libc::pid_t) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pid_cwd_matches_current_dir_for_self() {
        let expected = std::env::current_dir().unwrap();
        let actual = pid_cwd(unsafe { libc::getpid() }).unwrap();
        assert_eq!(expected, actual);
    }
}
