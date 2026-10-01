//! Generic weight-file detection (SPEC §10 "Generic"): which model weight files a process has open or
//! memory-mapped. llama.cpp / safetensors loaders `mmap` weights and may close the descriptor, so both open
//! files and mapped regions are checked.
//!
//! - macOS: `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)` for open files and
//!   `proc_pidinfo(PROC_PIDREGIONPATHINFO2)` for vnode-backed mappings (same-user processes only, no root).
//! - Linux: `/proc/<pid>/maps` and `/proc/<pid>/fd/*` (any root, so fixtures work).
//!
//! Only paths are read; weights are never opened here (sizes come from `stat`).

use crate::AdapterError;
use std::path::Path;
use std::time::{Duration, Instant};

/// Files smaller than this are not treated as weights (tokenizers, configs, small adapters).
pub const MIN_WEIGHT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Upper bound on mapped regions walked per process (a browser renderer has a few thousand).
pub const MAX_REGIONS: usize = 50_000;

/// Extensions that are weights wherever they appear.
const WEIGHT_EXTS: &[&str] = &[
    "gguf",
    "ggml",
    "safetensors",
    "ckpt",
    "pt",
    "pth",
    "onnx",
    "mlpackage",
    "npz",
    "llamafile",
];

/// True if `path` looks like a model weight file (by name only; see [`MIN_WEIGHT_FILE_BYTES`]).
/// `.bin` counts only with a model-ish name or location (`pytorch_model*.bin`, `ggml-*.bin`, `…/models/…`),
/// Ollama blobs (`…/models/blobs/sha256-…`) count without extension. System paths never count.
pub fn is_weight_path(path: &str) -> bool {
    if path.starts_with("/System/") || path.starts_with("/usr/lib/") || path.starts_with("/usr/share/icu") {
        return false;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if path.contains("/models/blobs/sha256-") || path.contains("/models/blobs/sha256:") {
        return true;
    }
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    if WEIGHT_EXTS.contains(&ext.as_str()) {
        return true;
    }
    if ext == "bin" {
        let s = stem.to_ascii_lowercase();
        return s.starts_with("pytorch_model")
            || s.starts_with("ggml-")
            || s.starts_with("model")
            || s.contains("gguf")
            || path.contains("/models/")
            || path.contains("/huggingface/");
    }
    false
}

fn keep(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = paths
        .into_iter()
        .filter(|p| is_weight_path(p))
        .filter(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.len() >= MIN_WEIGHT_FILE_BYTES)
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Paths of file-backed mappings in a `/proc/<pid>/maps` text (deleted files are skipped).
pub fn parse_maps_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        // address perms offset dev inode pathname
        let mut rest = line;
        let mut ok = true;
        for _ in 0..5 {
            rest = rest.trim_start();
            match rest.find(char::is_whitespace) {
                Some(i) => rest = &rest[i..],
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let path = rest.trim();
        if !path.starts_with('/') || path.ends_with(" (deleted)") {
            continue;
        }
        if !out.iter().any(|p: &String| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

/// Weight files open or mapped by `pid`, read from a procfs rooted at `root` (normally `/proc`).
pub fn scan_procfs(root: &Path, pid: u32) -> Result<Vec<String>, AdapterError> {
    scan_procfs_within(root, pid, Duration::from_secs(1))
}

/// [`scan_procfs`] with a budget for the per-descriptor `readlink` walk (maps are read whole).
pub fn scan_procfs_within(root: &Path, pid: u32, budget: Duration) -> Result<Vec<String>, AdapterError> {
    let end = Instant::now() + budget;
    let dir = root.join(pid.to_string());
    let maps = std::fs::read_to_string(dir.join("maps"))
        .map_err(|e| AdapterError::Io(format!("{}: {e}", dir.display())))?;
    let mut paths = parse_maps_paths(&maps);
    if let Ok(rd) = std::fs::read_dir(dir.join("fd")) {
        for e in rd.flatten().take(65_536) {
            if Instant::now() > end {
                break;
            }
            if let Ok(t) = std::fs::read_link(e.path()) {
                if let Some(s) = t.to_str() {
                    if s.starts_with('/') && !s.ends_with(" (deleted)") {
                        paths.push(s.to_string());
                    }
                }
            }
        }
    }
    Ok(keep(paths))
}

/// Weight files open or mapped by a live process on this machine. `budget` bounds the region walk.
pub fn scan_process(pid: u32, budget: Duration) -> Result<Vec<String>, AdapterError> {
    #[cfg(target_os = "macos")]
    {
        mac::scan(pid, budget).map(keep)
    }
    #[cfg(target_os = "linux")]
    {
        scan_procfs_within(Path::new("/proc"), pid, budget)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pid, budget);
        Err(AdapterError::Unsupported)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    //! `libproc` structures not exported by the `libc` crate, declared per `<sys/proc_info.h>`.
    use super::*;
    use libc::{c_int, c_void};
    use std::mem::{size_of, zeroed};

    const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    /// Like `PROC_PIDREGIONPATHINFO` but only returns regions backed by a vnode.
    const PROC_PIDREGIONPATHINFO2: c_int = 22;

    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: libc::vnode_info_path,
    }

    #[repr(C)]
    struct ProcRegionInfo {
        pri_protection: u32,
        pri_max_protection: u32,
        pri_inheritance: u32,
        pri_flags: u32,
        pri_offset: u64,
        pri_behavior: u32,
        pri_user_wired_count: u32,
        pri_user_tag: u32,
        pri_pages_resident: u32,
        pri_pages_shared_now_private: u32,
        pri_pages_swapped_out: u32,
        pri_pages_dirtied: u32,
        pri_ref_count: u32,
        pri_shadow_depth: u32,
        pri_share_mode: u32,
        pri_private_pages_resident: u32,
        pri_shared_pages_resident: u32,
        pri_obj_id: u32,
        pri_depth: u32,
        pri_address: u64,
        pri_size: u64,
    }

    #[repr(C)]
    struct ProcRegionWithPathInfo {
        prp_prinfo: ProcRegionInfo,
        prp_vip: libc::vnode_info_path,
    }

    fn path_of(vip: &libc::vnode_info_path) -> Option<String> {
        // `vip_path` is `[[c_char; 32]; 32]` in libc (a 1024-byte C string).
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(vip.vip_path.as_ptr() as *const u8, 1024) };
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        if end == 0 {
            return None;
        }
        std::str::from_utf8(&bytes[..end]).ok().map(str::to_string)
    }

    fn open_files(pid: u32, end: Instant) -> Result<Vec<String>, AdapterError> {
        let pid = pid as c_int;
        let needed = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Err(AdapterError::Io(format!(
                "proc_pidinfo(LISTFDS): {}",
                std::io::Error::last_os_error()
            )));
        }
        let cap = needed as usize / size_of::<libc::proc_fdinfo>() + 32;
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(cap);
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut c_void,
                (cap * size_of::<libc::proc_fdinfo>()) as c_int,
            )
        };
        if got <= 0 {
            return Err(AdapterError::Io("proc_pidinfo(LISTFDS) failed".into()));
        }
        unsafe { fds.set_len((got as usize / size_of::<libc::proc_fdinfo>()).min(cap)) };
        let mut out = Vec::new();
        for fd in fds
            .iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        {
            if Instant::now() > end {
                break;
            }
            let mut info: VnodeFdInfoWithPath = unsafe { zeroed() };
            let n = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEPATHINFO,
                    &mut info as *mut _ as *mut c_void,
                    size_of::<VnodeFdInfoWithPath>() as c_int,
                )
            };
            if n as usize == size_of::<VnodeFdInfoWithPath>() {
                if let Some(p) = path_of(&info.pvip) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    fn mapped_files(pid: u32, end: Instant) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut addr: u64 = 0;
        for _ in 0..MAX_REGIONS {
            if Instant::now() > end {
                break;
            }
            let mut info: ProcRegionWithPathInfo = unsafe { zeroed() };
            let n = unsafe {
                libc::proc_pidinfo(
                    pid as c_int,
                    PROC_PIDREGIONPATHINFO2,
                    addr,
                    &mut info as *mut _ as *mut c_void,
                    size_of::<ProcRegionWithPathInfo>() as c_int,
                )
            };
            if n as usize != size_of::<ProcRegionWithPathInfo>() {
                break;
            }
            if let Some(p) = path_of(&info.prp_vip) {
                if out.last() != Some(&p) && !out.contains(&p) {
                    out.push(p);
                }
            }
            let next = info
                .prp_prinfo
                .pri_address
                .saturating_add(info.prp_prinfo.pri_size);
            if next <= addr {
                break;
            }
            addr = next;
        }
        out
    }

    /// Open files, then mapped regions, sharing one deadline. Weights are usually mapped, so a failed fd
    /// listing does not discard the region walk; the scan fails only when neither yields anything.
    pub fn scan(pid: u32, budget: Duration) -> Result<Vec<String>, AdapterError> {
        let end = Instant::now() + budget;
        let open = open_files(pid, end);
        let mapped = mapped_files(pid, end);
        match open {
            Ok(mut paths) => {
                paths.extend(mapped);
                Ok(paths)
            }
            Err(_) if !mapped.is_empty() => Ok(mapped),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_paths() {
        for yes in [
            "/Users/u/models/qwen-image-2.1-UC-Q4_K_M.gguf",
            "/m/model-00001-of-00004.safetensors",
            "/Users/u/.ollama/models/blobs/sha256-6a0746a1ec1aef3e7ec53868f220ff6e389f6f8ef87a01d77c96807de94ca2aa",
            "/hf/models--x--y/snapshots/abc/pytorch_model.bin",
            "/x/ggml-large-v3.bin",
            "/x/models/foo/weights.bin",
            "/x/sd_xl_base_1.0.ckpt",
        ] {
            assert!(is_weight_path(yes), "{yes}");
        }
        for no in [
            "/System/Library/foo.onnx",
            "/usr/lib/x86_64-linux-gnu/libfoo.so",
            "/Applications/Google Chrome.app/Contents/Frameworks/x/resources.pak",
            "/tmp/data.bin",
            "/x/tokenizer.json",
            "/x/noext",
        ] {
            assert!(!is_weight_path(no), "{no}");
        }
    }

    #[test]
    fn maps_parsing() {
        let maps = "\
55d0c0a00000-55d0c0a21000 r--p 00000000 fd:01 1234  /usr/bin/python3.12
7f0000000000-7f0100000000 r--s 00000000 fd:01 99    /data/models/My Model Q4.gguf
7f0100000000-7f0200000000 r--s 00000000 fd:01 98    /data/old.gguf (deleted)
7ffd0000-7ffd1000 rw-p 00000000 00:00 0             [stack]
7ffe0000-7ffe1000 rw-p 00000000 00:00 0
7f0000000000-7f0100000000 r--s 00000000 fd:01 99    /data/models/My Model Q4.gguf
";
        assert_eq!(
            parse_maps_paths(maps),
            vec![
                "/usr/bin/python3.12".to_string(),
                "/data/models/My Model Q4.gguf".to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn procfs_fixture() {
        let root = tempfile::tempdir().unwrap();
        let models = tempfile::tempdir().unwrap();
        let big = models.path().join("llama-3-8b.Q4_K_M.gguf");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MIN_WEIGHT_FILE_BYTES + 1)
            .unwrap();
        let small = models.path().join("lora.gguf");
        std::fs::File::create(&small).unwrap().set_len(1024).unwrap();
        let st = models.path().join("model.safetensors");
        std::fs::File::create(&st)
            .unwrap()
            .set_len(MIN_WEIGHT_FILE_BYTES)
            .unwrap();
        let pd = root.path().join("4242");
        std::fs::create_dir_all(pd.join("fd")).unwrap();
        std::fs::write(
            pd.join("maps"),
            format!(
                "7f00-7f01 r--s 00000000 fd:01 1 {}\n7f01-7f02 r--s 00000000 fd:01 2 {}\n",
                big.display(),
                small.display()
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(&st, pd.join("fd").join("7")).unwrap();
        std::os::unix::fs::symlink("/dev/null", pd.join("fd").join("0")).unwrap();
        let mut want = vec![big.display().to_string(), st.display().to_string()];
        want.sort();
        assert_eq!(scan_procfs(root.path(), 4242).unwrap(), want);
        assert!(
            scan_procfs(root.path(), 1).is_err(),
            "missing pid → error, no panic"
        );
    }

    #[test]
    fn scan_of_missing_process_is_an_error_not_a_panic() {
        // pid_max is well below this on both OSes.
        assert!(scan_process(999_999_999, Duration::from_millis(50)).is_err());
        // A zero budget still returns promptly (possibly empty).
        let t = Instant::now();
        let _ = scan_process(std::process::id(), Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn procfs_fd_walk_honours_budget() {
        let root = tempfile::tempdir().unwrap();
        let models = tempfile::tempdir().unwrap();
        let big = models.path().join("w.gguf");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MIN_WEIGHT_FILE_BYTES)
            .unwrap();
        let pd = root.path().join("7");
        std::fs::create_dir_all(pd.join("fd")).unwrap();
        std::fs::write(pd.join("maps"), "").unwrap();
        std::os::unix::fs::symlink(&big, pd.join("fd").join("3")).unwrap();
        assert_eq!(
            scan_procfs_within(root.path(), 7, Duration::from_secs(1)).unwrap(),
            vec![big.display().to_string()]
        );
        assert!(scan_procfs_within(root.path(), 7, Duration::ZERO)
            .unwrap()
            .is_empty());
    }

    /// Opens and maps a sparse weight file in this test process, then finds it through the OS APIs.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn scans_own_process_for_open_and_mapped_weights() {
        use std::os::unix::io::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let open_path = dir.path().join("open-model.gguf");
        let open_file = std::fs::File::create(&open_path).unwrap();
        open_file.set_len(MIN_WEIGHT_FILE_BYTES).unwrap();
        let map_path = dir.path().join("mapped-model.safetensors");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&map_path)
            .unwrap();
        f.set_len(MIN_WEIGHT_FILE_BYTES).unwrap();
        let len = 1024 * 1024;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        drop(f); // mapping survives the descriptor, like llama.cpp after loading
        let found = scan_process(std::process::id(), Duration::from_secs(2)).unwrap();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap().display().to_string();
        assert!(found.contains(&canon(&open_path)), "{found:?}");
        assert!(found.contains(&canon(&map_path)), "{found:?}");
        unsafe { libc::munmap(ptr, len) };
        drop(open_file);
    }
}
