//! Process-group control for agent runs. Each run is started in its own
//! process group, so killing the group also stops any shell or child it spawned.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// SIGKILL to the process group whose leader is `pid`.
pub fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    // SAFETY: killpg only sends a signal. A stale pid at worst hits a group
    // that no longer exists, which returns ESRCH and is ignored.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

/// True while a process with this pid exists.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs the existence check only.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The last `max` bytes of a log file, as text.
pub fn read_tail(path: &Path, max: u64) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max && file.seek(SeekFrom::Start(len - max)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Time since the file was last written. Used to spot a run that has gone quiet.
pub fn idle_for(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}
