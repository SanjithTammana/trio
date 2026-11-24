//! Ownership of an agent run's whole process tree, kept until the run is cleaned up.
//!
//! A CLI agent starts helpers (MCP servers, shells) that can outlive it. Killing only the direct
//! child, or forgetting the run once it exits, would leave them running unowned.
//!
//! - Windows: the child is created suspended, joins a Job Object made with KILL_ON_JOB_CLOSE, and
//!   only then runs, so nothing it starts can escape the job. Terminating or dropping the job ends
//!   every process in it, including ones whose parent already exited. If the job cannot be made
//!   or joined, the suspended child is killed and the spawn fails: a run is never started unowned.
//! - Unix: the child leads its own process group (`process_group(0)` at spawn), and the group
//!   outlives its leader, so `kill -KILL -<pgid>` reaches orphans too.

pub struct Tree {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: job::Job,
    done: bool,
}

/// Spawn `cmd` as the root of an owned process tree.
pub fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<(tokio::process::Child, Tree)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        cmd.creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        let owned = job::Job::new().filter(|j| child.raw_handle().is_some_and(|h| j.assign(h)) && child.id().is_some_and(job::resume));
        match owned {
            Some(job) => Ok((child, Tree { job, done: false })),
            None => {
                let _ = child.start_kill();
                Err(std::io::Error::other("could not place the run in a job object; it was not started"))
            }
        }
    }
    #[cfg(unix)]
    {
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pid = child.id();
        Ok((child, Tree { pid, done: false }))
    }
}

impl Tree {
    /// Kill everything still in the tree. Idempotent.
    pub fn kill(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(windows)]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    pub struct Job(HANDLE);

    // The handle is a kernel object reference, usable from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn new() -> Option<Self> {
            // SAFETY: plain Win32 calls with valid pointers; the handle is closed on every path.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    CloseHandle(h);
                    return None;
                }
                Some(Self(h))
            }
        }

        pub fn assign(&self, process: std::os::windows::io::RawHandle) -> bool {
            // SAFETY: `process` is the live child's handle, owned by its tokio Child.
            unsafe { AssignProcessToJobObject(self.0, process as HANDLE) != 0 }
        }

        pub fn terminate(&self) {
            // SAFETY: self.0 is a valid job handle until drop.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing our own handle once; KILL_ON_JOB_CLOSE ends what is left.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Resume the threads of a process created suspended (only its main thread exists yet).
    /// True when at least one thread was resumed and none failed.
    pub fn resume(pid: u32) -> bool {
        // SAFETY: a thread snapshot walked with a correctly sized entry; every handle is closed.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let (mut resumed, mut failed) = (0, false);
            let mut more = Thread32First(snap, &mut e) != 0;
            while more {
                if e.th32OwnerProcessID == pid {
                    let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                    if t.is_null() || ResumeThread(t) == u32::MAX {
                        failed = true;
                    } else {
                        resumed += 1;
                    }
                    if !t.is_null() {
                        CloseHandle(t);
                    }
                }
                more = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
            resumed > 0 && !failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A grandchild that outlives its parent keeps appending to a file; killing the tree must
    /// stop it even though the direct child already exited.
    #[tokio::test]
    async fn an_orphaned_grandchild_dies_with_the_tree() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let f = std::env::temp_dir().join(format!("trio-tree-{}-{nanos}.txt", std::process::id()));
        let fs = f.display().to_string();
        #[cfg(windows)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("cmd");
            let loop_ps = format!("while($true){{Add-Content -LiteralPath '{fs}' x; Start-Sleep -Milliseconds 100}}");
            c.args(["/C", "start", "", "/B", "powershell", "-NoProfile", "-Command", &loop_ps]);
            c
        };
        #[cfg(unix)]
        let mut cmd = {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", &format!("(while :; do echo x >> '{fs}'; sleep 0.1; done) & exit 0")]);
            c
        };
        cmd.stdout(std::process::Stdio::null());
        let (mut child, mut tree) = spawn(&mut cmd).unwrap();
        assert!(child.wait().await.unwrap().success(), "the direct child was resumed and exits at once");
        let len = || std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        // Wait for the grandchild to be writing.
        for _ in 0..100 {
            if len() > 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(len() > 2, "grandchild never started");
        tree.kill();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let after = len();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(len(), after, "the grandchild kept running after the tree was killed");
        let _ = std::fs::remove_file(&f);
    }
}
