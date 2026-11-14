use std::sync::Arc;

pub use crate::workflow::code_runtime::MEMORY_CAP_BYTES;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MemoryCapUnavailableError {
    message: String,
}

impl MemoryCapUnavailableError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait MemoryGuard {
    fn exceeded(&self) -> bool;
    fn dispose(&mut self);
}

pub fn parse_linux_status_rss(text: &str) -> Option<usize> {
    text.lines().find_map(|line| {
        let value = trim_start_js(line.strip_prefix("VmRSS:")?);
        let digits = value
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits == 0 {
            return None;
        }
        if trim_start_js(&value[digits..]) != "kB" {
            return None;
        }
        value[..digits].parse::<usize>().ok()?.checked_mul(1024)
    })
}

pub fn create_memory_guard(
    pid: u32,
    cap_bytes: usize,
    kill: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn MemoryGuard>, MemoryCapUnavailableError> {
    #[cfg(windows)]
    {
        let _ = kill;
        return create_windows_guard(pid, cap_bytes).map(|guard| Box::new(guard) as Box<dyn MemoryGuard>);
    }
    #[cfg(target_os = "linux")]
    {
        return create_linux_guard(pid, cap_bytes, kill).map(|guard| Box::new(guard) as Box<dyn MemoryGuard>);
    }
    #[cfg(target_os = "macos")]
    {
        return create_darwin_guard(pid, cap_bytes, kill).map(|guard| Box::new(guard) as Box<dyn MemoryGuard>);
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, cap_bytes, kill);
        Err(MemoryCapUnavailableError::new(format!(
            "no memory cap enforcement is implemented for platform {}",
            std::env::consts::OS
        )))
    }
}

#[cfg(target_os = "linux")]
struct LinuxMemoryGuard {
    hit: Arc<std::sync::atomic::AtomicBool>,
    disposed: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl MemoryGuard for LinuxMemoryGuard {
    fn exceeded(&self) -> bool {
        self.hit.load(std::sync::atomic::Ordering::Acquire)
    }

    fn dispose(&mut self) {
        self.disposed.store(true, std::sync::atomic::Ordering::Release);
        self.worker.take();
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxMemoryGuard {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(target_os = "linux")]
fn create_linux_guard(
    pid: u32,
    cap_bytes: usize,
    kill: Arc<dyn Fn() + Send + Sync>,
) -> Result<LinuxMemoryGuard, MemoryCapUnavailableError> {
    let status_path = format!("/proc/{pid}/status");
    std::fs::read_to_string(&status_path).map_err(|_| {
        MemoryCapUnavailableError::new(format!(
            "/proc is not readable for pid {pid}; cannot enforce a memory cap"
        ))
    })?;
    let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let disposed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = std::thread::spawn({
        let hit = hit.clone();
        let disposed = disposed.clone();
        move || {
            while !disposed.load(std::sync::atomic::Ordering::Acquire)
                && !hit.load(std::sync::atomic::Ordering::Acquire)
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
                let Ok(text) = std::fs::read_to_string(&status_path) else {
                    continue;
                };
                if parse_linux_status_rss(&text).is_some_and(|rss| rss > cap_bytes) {
                    hit.store(true, std::sync::atomic::Ordering::Release);
                    kill();
                }
            }
        }
    });
    Ok(LinuxMemoryGuard {
        hit,
        disposed,
        worker: Some(worker),
    })
}

#[cfg(target_os = "macos")]
struct DarwinMemoryGuard {
    hit: Arc<std::sync::atomic::AtomicBool>,
    disposed: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl MemoryGuard for DarwinMemoryGuard {
    fn exceeded(&self) -> bool {
        self.hit.load(std::sync::atomic::Ordering::Acquire)
    }

    fn dispose(&mut self) {
        self.disposed.store(true, std::sync::atomic::Ordering::Release);
        self.worker.take();
    }
}

#[cfg(target_os = "macos")]
impl Drop for DarwinMemoryGuard {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(target_os = "macos")]
fn create_darwin_guard(
    pid: u32,
    cap_bytes: usize,
    kill: Arc<dyn Fn() + Send + Sync>,
) -> Result<DarwinMemoryGuard, MemoryCapUnavailableError> {
    probe_darwin_ps(pid)?;
    let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let disposed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = std::thread::spawn({
        let hit = hit.clone();
        let disposed = disposed.clone();
        move || {
            while !disposed.load(std::sync::atomic::Ordering::Acquire)
                && !hit.load(std::sync::atomic::Ordering::Acquire)
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let Ok(output) = std::process::Command::new("ps")
                    .args(["-o", "rss=", "-p", &pid.to_string()])
                    .output()
                else {
                    continue;
                };
                let rss = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|kb| kb.checked_mul(1024));
                if rss.is_some_and(|rss| rss > cap_bytes) {
                    hit.store(true, std::sync::atomic::Ordering::Release);
                    kill();
                }
            }
        }
    });
    Ok(DarwinMemoryGuard {
        hit,
        disposed,
        worker: Some(worker),
    })
}

#[cfg(target_os = "macos")]
fn probe_darwin_ps(pid: u32) -> Result<(), MemoryCapUnavailableError> {
    let status = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| MemoryCapUnavailableError::new("ps is not usable on this host; cannot enforce a memory cap"))?;
    if status.success() {
        Ok(())
    } else {
        Err(MemoryCapUnavailableError::new("ps is not usable on this host; cannot enforce a memory cap"))
    }
}

#[cfg(windows)]
struct WindowsMemoryGuard {
    job: usize,
    process: usize,
    cap_bytes: usize,
    last_peak: std::sync::atomic::AtomicUsize,
    disposed: bool,
}

#[cfg(windows)]
impl MemoryGuard for WindowsMemoryGuard {
    fn exceeded(&self) -> bool {
        let mut info = [0_u8; WIN32_EXTENDED_LIMIT_SIZE];
        let ok = unsafe {
            QueryInformationJobObject(
                self.job as *mut _,
                WIN32_EXTENDED_LIMIT_INFO_CLASS,
                info.as_mut_ptr().cast(),
                WIN32_EXTENDED_LIMIT_SIZE as u32,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            let peak = u64::from_ne_bytes(info[WIN32_PEAK_JOB_MEMORY_OFFSET..WIN32_PEAK_JOB_MEMORY_OFFSET + 8].try_into().unwrap()) as usize;
            self.last_peak.store(peak, std::sync::atomic::Ordering::Release);
        }
        self.last_peak.load(std::sync::atomic::Ordering::Acquire) >= self.cap_bytes
    }

    fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        unsafe {
            CloseHandle(self.job as *mut _);
            CloseHandle(self.process as *mut _);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsMemoryGuard {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(windows)]
const WIN32_EXTENDED_LIMIT_INFO_CLASS: i32 = 9;
#[cfg(windows)]
const WIN32_LIMIT_PROCESS_MEMORY: u32 = 0x00000100;
#[cfg(windows)]
const WIN32_LIMIT_JOB_MEMORY: u32 = 0x00000200;
#[cfg(windows)]
const WIN32_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;
#[cfg(windows)]
const WIN32_PROCESS_SET_QUOTA: u32 = 0x0100;
#[cfg(windows)]
const WIN32_PROCESS_TERMINATE: u32 = 0x0001;
#[cfg(windows)]
const WIN32_PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const WIN32_EXTENDED_LIMIT_SIZE: usize = 144;
#[cfg(windows)]
const WIN32_PEAK_JOB_MEMORY_OFFSET: usize = 136;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *mut core::ffi::c_void, name: *const u16) -> *mut core::ffi::c_void;
    fn SetInformationJobObject(
        job: *mut core::ffi::c_void,
        info_class: i32,
        info: *const core::ffi::c_void,
        size: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: *mut core::ffi::c_void, process: *mut core::ffi::c_void) -> i32;
    fn QueryInformationJobObject(
        job: *mut core::ffi::c_void,
        info_class: i32,
        info: *mut core::ffi::c_void,
        size: u32,
        return_length: *mut u32,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

#[cfg(windows)]
fn create_windows_guard(pid: u32, cap_bytes: usize) -> Result<WindowsMemoryGuard, MemoryCapUnavailableError> {
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() {
        return Err(MemoryCapUnavailableError::new("CreateJobObjectW failed"));
    }
    let mut info = [0_u8; WIN32_EXTENDED_LIMIT_SIZE];
    info[16..20].copy_from_slice(
        &(WIN32_LIMIT_PROCESS_MEMORY | WIN32_LIMIT_JOB_MEMORY | WIN32_LIMIT_KILL_ON_JOB_CLOSE)
            .to_ne_bytes(),
    );
    info[112..120].copy_from_slice(&(cap_bytes as u64).to_ne_bytes());
    info[120..128].copy_from_slice(&(cap_bytes as u64).to_ne_bytes());
    let process = unsafe {
        if SetInformationJobObject(
            job,
            WIN32_EXTENDED_LIMIT_INFO_CLASS,
            info.as_ptr().cast(),
            WIN32_EXTENDED_LIMIT_SIZE as u32,
        ) == 0 {
            CloseHandle(job);
            return Err(MemoryCapUnavailableError::new(
                "SetInformationJobObject rejected the memory limits",
            ));
        }
        OpenProcess(
            WIN32_PROCESS_SET_QUOTA | WIN32_PROCESS_TERMINATE | WIN32_PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if process.is_null() {
        unsafe { CloseHandle(job) };
        return Err(MemoryCapUnavailableError::new(format!("OpenProcess failed for pid {pid}")));
    }
    if unsafe { AssignProcessToJobObject(job, process) } == 0 {
        unsafe {
            CloseHandle(job);
            CloseHandle(process);
        }
        return Err(MemoryCapUnavailableError::new(format!(
            "AssignProcessToJobObject failed for pid {pid}"
        )));
    }
    Ok(WindowsMemoryGuard {
        job: job as usize,
        process: process as usize,
        cap_bytes,
        last_peak: std::sync::atomic::AtomicUsize::new(0),
        disposed: false,
    })
}

fn trim_start_js(value: &str) -> &str {
    value.trim_start_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
