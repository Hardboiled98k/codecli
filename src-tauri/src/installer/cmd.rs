// SPDX-License-Identifier: MPL-2.0
//! 带超时的子进程执行（防管道死锁 + 可取消）

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

static CANCEL: AtomicBool = AtomicBool::new(false);

/// stdout/stderr 各自最多保留 4 MiB。读线程仍会继续排空后续输出，
/// 避免子进程因 pipe 填满而死锁。
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const TRUNCATED_SUFFIX: &str = "\n[输出已截断（上限 4 MiB）；其余内容已排空]\n";
/// 直接 child 退出后，pipe 应该很快 EOF。超过这个宽限通常
/// 表示 shell/npm 留下了仍持有 stdout/stderr 的后台后代。
const OUTPUT_EOF_GRACE: Duration = Duration::from_secs(1);

pub fn request_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

pub fn clear_cancel() {
    CANCEL.store(false, Ordering::SeqCst);
}

pub fn is_cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

pub fn check_cancelled() -> Result<(), String> {
    if is_cancelled() {
        Err("用户已取消操作".into())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub struct TimedOutput {
    pub status_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

fn truncate_on_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// 持续读到 EOF，但只保留前 `limit` 字节。最终 String（含截断提示）
/// 也不会超过 `limit` 字节，包括非 UTF-8 输出的有损转换场景。
fn read_bounded<R: Read>(mut reader: R, limit: usize) -> String {
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(captured.len());
                let keep = remaining.min(read);
                captured.extend_from_slice(&chunk[..keep]);
                if keep < read {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let decoded = String::from_utf8_lossy(&captured);
    // from_utf8_lossy 可能把一个无效字节扩展成三字节替换符，因此还需
    // 对最终 UTF-8 String 再做一次字节上限检查。
    truncated |= decoded.len() > limit;
    if !truncated {
        return decoded.into_owned();
    }

    if limit == 0 {
        return String::new();
    }

    let suffix = truncate_on_char_boundary(TRUNCATED_SUFFIX, limit);
    if suffix.len() == limit {
        return suffix.to_string();
    }

    let payload_limit = limit - suffix.len();
    let payload = truncate_on_char_boundary(decoded.as_ref(), payload_limit);
    let mut output = String::with_capacity(payload.len() + suffix.len());
    output.push_str(payload);
    output.push_str(suffix);
    output
}

fn receive_reader_before(receiver: &mpsc::Receiver<String>, deadline: Instant) -> Option<String> {
    match receiver.try_recv() {
        Ok(output) => return Some(output),
        Err(mpsc::TryRecvError::Disconnected) => return Some(String::new()),
        Err(mpsc::TryRecvError::Empty) => {}
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(output) => Some(output),
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(String::new()),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
    }
}

#[cfg(unix)]
fn configure_process_tree(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // 让 child 成为新 process group 的 leader。超时/取消时 killpg 才能同时
    // 终止 npm/shell 等命令再拉起的后代进程。
    cmd.process_group(0);
}

#[cfg(windows)]
fn configure_process_tree(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    // 让初始线程在任何用户代码执行前暂停。spawn 返回后先把
    // process 分配进 Job，再显式 ResumeThread，消除普通
    // spawn -> AssignProcessToJobObject 之间可拉起逃逸后代的竞态窗口。
    cmd.creation_flags(CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_process_tree(_cmd: &mut Command) {}

#[cfg(windows)]
struct WindowsTemporaryHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsTemporaryHandle {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // SAFETY: 每个临时 HANDLE 在构造前已验证有效，并且只在此处关闭。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
struct WindowsJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl WindowsJob {
    fn create_kill_on_close() -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: null SECURITY_ATTRIBUTES/name 创建当前进程专用的匿名 Job。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "创建 Windows Job Object 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let job = Self { handle };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: handle 属于 job；limits 的类型、长度与
        // JobObjectExtendedLimitInformation 严格匹配。
        let configured = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(format!(
                "配置 Windows Job Object KILL_ON_JOB_CLOSE 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        // SAFETY: child raw handle 在 Child 存活期间有效；Job handle 由 self 持有。
        let assigned = unsafe {
            AssignProcessToJobObject(
                self.handle,
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            )
        };
        if assigned == 0 {
            return Err(format!(
                "将子进程分配到 Windows Job Object 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn resume_suspended_child(child: &Child) -> Result<(), String> {
        use windows_sys::Win32::Foundation::{
            GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        // CREATE_SUSPENDED 返回时目标不能执行用户代码，因此只应
        // 存在一个初始线程。用 ToolHelp 找到该线程后才恢复。
        // SAFETY: 参数是文档规定的 TH32CS_SNAPTHREAD + 0。
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return Err(format!(
                "创建 Windows 线程快照失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let snapshot = WindowsTemporaryHandle(snapshot);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut thread_ids = Vec::new();
        // SAFETY: snapshot 有效，entry 大小已初始化且可写。
        if unsafe { Thread32First(snapshot.0, &raw mut entry) } == 0 {
            return Err(format!(
                "枚举 Windows 初始线程失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        loop {
            if entry.th32OwnerProcessID == child.id() {
                thread_ids.push(entry.th32ThreadID);
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            // SAFETY: 同上；返回 0 时用 GetLastError 区分正常枚举结束与失败。
            if unsafe { Thread32Next(snapshot.0, &raw mut entry) } == 0 {
                // SAFETY: 紧跟失败的 Thread32Next 读取 thread-local last error。
                let error = unsafe { GetLastError() };
                if error != ERROR_NO_MORE_FILES {
                    return Err(format!("继续枚举 Windows 初始线程失败: OS error {error}"));
                }
                break;
            }
        }
        if thread_ids.len() != 1 {
            return Err(format!(
                "CREATE_SUSPENDED 子进程的初始线程数异常: {}",
                thread_ids.len()
            ));
        }

        // SAFETY: thread id 来自尚未恢复的目标进程快照。
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_ids[0]) };
        if thread.is_null() {
            return Err(format!(
                "打开 Windows 初始线程失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        let thread = WindowsTemporaryHandle(thread);
        // ResumeThread 失败返回 u32::MAX；CREATE_SUSPENDED 的初始计数
        // 应精确为 1。0 意味着进程已可能执行，>1 意味着它仍暂停，
        // 两者都不能降级继续。
        // SAFETY: thread handle 具有 THREAD_SUSPEND_RESUME 权限。
        let previous_count = unsafe { ResumeThread(thread.0) };
        if previous_count != 1 {
            return Err(format!(
                "恢复 Windows 初始线程失败或暂停计数异常: {previous_count}"
            ));
        }
        Ok(())
    }

    fn active_processes(&self) -> Result<u32, String> {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };

        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: accounting 类型、长度与 JobObjectBasicAccountingInformation
        // 严格匹配，输出缓冲在调用期间有效。
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(format!(
                "查询 Windows Job Object 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(accounting.ActiveProcesses)
    }

    fn terminate(&self) -> Result<(), String> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        // SAFETY: handle 是当前存活的 Job Object。
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            return Err(format!(
                "终止 Windows Job Object 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // KILL_ON_JOB_CLOSE 是最后一层保障：无论 run_timed 从哪个
        // 错误分支返回，关闭 Job 都会终止尚未退出的成员。
        // SAFETY: handle 只由本对象拥有并在 Drop 中关闭一次。
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

struct ProcessTreeGuard {
    #[cfg(windows)]
    job: WindowsJob,
}

impl ProcessTreeGuard {
    fn prepare() -> Result<Self, String> {
        #[cfg(windows)]
        {
            Ok(Self {
                job: WindowsJob::create_kill_on_close()?,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        #[cfg(windows)]
        {
            self.job.assign(child)?;
            WindowsJob::resume_suspended_child(child)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(())
        }
    }

    fn has_live_descendants(&self, process_group: u32) -> Result<bool, String> {
        #[cfg(unix)]
        {
            let process_group = process_group as libc::pid_t;
            if process_group <= 0 {
                return Err("子进程组 ID 无效".into());
            }
            // 直接 child 已被 try_wait 回收；此时该 PGID 仍存在就说明
            // 至少有一个后代存活，即使它已把 stdout/stderr 重定向。
            // SAFETY: signal 0 只检查进程组是否存在，不会修改进程。
            if unsafe { libc::killpg(process_group, 0) } == 0 {
                return Ok(true);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Ok(false),
                Some(libc::EPERM) => Ok(true),
                code => Err(format!("检查子进程组失败: {code:?}")),
            }
        }
        #[cfg(windows)]
        {
            let _ = process_group;
            self.job.active_processes().map(|active| active > 0)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = process_group;
            Ok(false)
        }
    }

    #[cfg(windows)]
    fn terminate_job(&self) -> Result<(), String> {
        self.job.terminate()
    }
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child, _guard: Option<&ProcessTreeGuard>) {
    let process_group = child.id() as libc::pid_t;
    if process_group > 0 {
        // SAFETY: process_group 来自我们刚刚 spawn 的 child PID，且 spawn 前已通过
        // process_group(0) 把 child 设为该进程组的 leader。
        unsafe {
            libc::killpg(process_group, libc::SIGKILL);
        }
    }
    // killpg 失败时至少终止直接 child；若已被 killpg 终止则这里无害。
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child, guard: Option<&ProcessTreeGuard>) {
    use std::path::{Path, PathBuf};

    // 一旦子进程已被 wait 回收，taskkill /T 无法再根据死 PID
    // 可靠枚举后代。因此优先终止 spawn 后立即绑定的 Job。
    if guard.is_some_and(|guard| guard.terminate_job().is_ok()) {
        let _ = child.kill();
        let _ = child.wait();
        return;
    }

    let pid = child.id().to_string();
    let system_root = std::env::var_os("SystemRoot")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let system32 = system_root.join("System32");
    let taskkill_exe = system32.join("taskkill.exe");

    // taskkill /T 能按父子关系终止后代进程。完全抑制它的输出，并只传
    // Windows 系统命令需要的最小环境，避免 API key 等密钥被二次传递。
    let mut killer = Command::new(taskkill_exe);
    killer
        .args(["/PID", &pid, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .env("SystemRoot", &system_root)
        .env("WINDIR", &system_root)
        .env("PATH", &system32);

    if let Ok(mut taskkill) = killer.spawn() {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match taskkill.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = taskkill.kill();
                    let _ = taskkill.wait();
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    // taskkill 不可用/失败时的最后保障。
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(child: &mut Child, _guard: Option<&ProcessTreeGuard>) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 运行命令：边跑边排空 stdout/stderr，避免管道填满死锁；超时/取消 kill
pub fn run_timed(mut cmd: Command, timeout_secs: u64) -> Result<TimedOutput, String> {
    check_cancelled()?;
    // npm/brew/winget/curl 等安装子进程不应继承用户配置中的 API Key。
    // 在统一执行入口处处理，避免新增调用点遗漏。
    super::util::strip_secret_env_from_command(&mut cmd);
    configure_process_tree(&mut cmd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // Windows Job 在 spawn 前先创建并配置；这样创建/配置失败时
    // 根本不启动子进程。spawn 后紧接着 assign，不执行其他业务。
    let process_tree = ProcessTreeGuard::prepare()?;
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {}", e))?;
    if let Err(error) = process_tree.assign(&child) {
        // Assign 失败包括不兼容的嵌套 Job。不能降级为无 Job
        // 运行，否则直接 child 退出后就失去可靠的后代句柄。
        terminate_process_tree(&mut child, Some(&process_tree));
        return Err(format!("{error}；已终止子进程，拒绝在无进程树保护下继续"));
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let (tx_out, rx_out) = mpsc::channel::<String>();
    let (tx_err, rx_err) = mpsc::channel::<String>();

    if let Some(mut out) = stdout_pipe {
        thread::spawn(move || {
            let _ = tx_out.send(read_bounded(&mut out, MAX_CAPTURE_BYTES));
        });
    } else {
        let _ = tx_out.send(String::new());
    }
    if let Some(mut err) = stderr_pipe {
        thread::spawn(move || {
            let _ = tx_err.send(read_bounded(&mut err, MAX_CAPTURE_BYTES));
        });
    } else {
        let _ = tx_err.send(String::new());
    }

    let start = Instant::now();
    let limit = Duration::from_secs(timeout_secs);

    let status = loop {
        if is_cancelled() {
            terminate_process_tree(&mut child, Some(&process_tree));
            // 尽量回收读线程
            let _ = rx_out.recv_timeout(Duration::from_millis(200));
            let _ = rx_err.recv_timeout(Duration::from_millis(200));
            return Err("用户已取消操作".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > limit {
                    terminate_process_tree(&mut child, Some(&process_tree));
                    let _ = rx_out.recv_timeout(Duration::from_millis(200));
                    let _ = rx_err.recv_timeout(Duration::from_millis(200));
                    return Err(format!(
                        "操作超时（{} 秒）。请检查网络后重试，或点取消后手动安装。",
                        timeout_secs
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                terminate_process_tree(&mut child, Some(&process_tree));
                let _ = rx_out.recv_timeout(Duration::from_millis(200));
                let _ = rx_err.recv_timeout(Duration::from_millis(200));
                return Err(format!("等待进程失败: {}", e));
            }
        }
    };

    // EOF 不能作为唯一的后代证明：后台进程可以把两个 pipe
    // 都重定向到 /dev/null/NUL。直接 child 已回收后，Unix 检查
    // PGID，Windows 查询 Job 的 ActiveProcesses；任何残余都终止并失败。
    match process_tree.has_live_descendants(child.id()) {
        Ok(true) => {
            terminate_process_tree(&mut child, Some(&process_tree));
            let _ = rx_out.recv_timeout(Duration::from_secs(1));
            let _ = rx_err.recv_timeout(Duration::from_secs(1));
            return Err("命令主进程已退出，但后台子进程仍存活；已终止进程树并拒绝视为成功".into());
        }
        Ok(false) => {}
        Err(error) => {
            terminate_process_tree(&mut child, Some(&process_tree));
            let _ = rx_out.recv_timeout(Duration::from_secs(1));
            let _ = rx_err.recv_timeout(Duration::from_secs(1));
            return Err(format!(
                "{error}；无法证明后台子进程已全部退出，已终止进程树"
            ));
        }
    }

    // 直接 child 已退出。如果读线程迟迟等不到 EOF，不能像过去
    // 那样在 5 秒后用空输出静默返回：这说明有后台后代持有 pipe，
    // 且该后代可能继续修改安装状态。终止整个进程树并明确失败。
    let output_deadline = Instant::now() + OUTPUT_EOF_GRACE;
    let mut stdout = receive_reader_before(&rx_out, output_deadline);
    let mut stderr = receive_reader_before(&rx_err, output_deadline);
    if stdout.is_none() || stderr.is_none() {
        terminate_process_tree(&mut child, Some(&process_tree));
        // killpg/taskkill 后给读线程一个很短的收尾窗口，避免在
        // 系统管道关闭通知尚未送达时遗留不必要的读线程。
        if stdout.is_none() {
            stdout = rx_out.recv_timeout(Duration::from_secs(1)).ok();
        }
        if stderr.is_none() {
            stderr = rx_err.recv_timeout(Duration::from_secs(1)).ok();
        }
        drop(stdout);
        drop(stderr);
        return Err(
            "命令主进程已退出，但后台子进程仍持有输出管道；已终止进程树并拒绝视为成功".into(),
        );
    }
    let stdout = stdout.unwrap_or_default();
    let stderr = stderr.unwrap_or_default();

    Ok(TimedOutput {
        status_ok: status.success(),
        stdout,
        stderr,
    })
}

pub fn humanize_npm_err(stderr: &str) -> String {
    let s = stderr.to_lowercase();
    if s.contains("eacces") || s.contains("permission denied") {
        return "权限不足（无法写全局 npm 目录）。将尝试用户级安装；若仍失败请用管理员重试或修复 npm 权限。".into();
    }
    if s.contains("enotfound") || s.contains("getaddrinfo") || s.contains("eai_again") {
        return "DNS/网络失败：无法解析 npm 源。请检查网络、代理或 DNS。".into();
    }
    if s.contains("etimedout") || s.contains("timeout") || s.contains("timed out") {
        return "下载超时：网络过慢或被墙。可开代理后重试，或切换镜像选项。".into();
    }
    if s.contains("cert") || s.contains("ssl") || s.contains("certificate") {
        return "证书/TLS 错误：公司网可能拦截 HTTPS。需配置系统/代理证书。".into();
    }
    if s.contains("407") || s.contains("proxy") {
        return "代理错误：请配置系统代理或关闭错误代理后重试。".into();
    }
    if s.contains("enospc") || s.contains("no space") {
        return "磁盘空间不足，请清理后重试。".into();
    }
    let short: String = stderr.chars().take(280).collect();
    if short.trim().is_empty() {
        "npm 失败（无详细输出）".into()
    } else {
        format!("npm 失败: {}", short)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    #[cfg(unix)]
    static PROCESS_TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn bounded_reader_drains_all_input_and_marks_truncation() {
        let input = vec![b'x'; MAX_CAPTURE_BYTES + 32 * 1024];
        let expected_len = input.len();
        let mut cursor = Cursor::new(input);

        let output = read_bounded(&mut cursor, MAX_CAPTURE_BYTES);

        assert_eq!(cursor.position() as usize, expected_len);
        assert!(output.ends_with(TRUNCATED_SUFFIX));
        assert!(output.len() <= MAX_CAPTURE_BYTES);
    }

    #[test]
    fn bounded_reader_preserves_small_output() {
        let input = "npm 安装完成\n".as_bytes();
        let output = read_bounded(Cursor::new(input), MAX_CAPTURE_BYTES);

        assert_eq!(output, "npm 安装完成\n");
        assert!(!output.contains("输出已截断"));
    }

    #[test]
    fn bounded_reader_stays_bounded_after_lossy_utf8_conversion() {
        let input = vec![0xff; MAX_CAPTURE_BYTES];
        let output = read_bounded(Cursor::new(input), MAX_CAPTURE_BYTES);

        assert!(output.ends_with(TRUNCATED_SUFFIX));
        assert!(output.len() <= MAX_CAPTURE_BYTES);
        assert!(output.is_char_boundary(output.len()));
    }

    #[cfg(unix)]
    fn assert_background_descendant_is_rejected(script: &str, label: &str) {
        clear_cancel();
        let pid_file = std::env::temp_dir().join(format!(
            "codecli-run-timed-descendant-{label}-{}-{}.pid",
            std::process::id(),
            PROCESS_TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let _ = std::fs::remove_file(&pid_file);

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .arg("codecli-run-timed-test")
            .arg(&pid_file);

        let started = Instant::now();
        let result = run_timed(command, 10);
        let elapsed = started.elapsed();
        let descendant_pid = std::fs::read_to_string(&pid_file)
            .expect("background descendant pid file")
            .trim()
            .parse::<libc::pid_t>()
            .expect("numeric descendant pid");

        let process_exists = |pid: libc::pid_t| {
            // SAFETY: signal 0 不会修改目标进程，只检查其是否存在。
            let result = unsafe { libc::kill(pid, 0) };
            result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        let gone_deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(descendant_pid) && Instant::now() < gone_deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let descendant_still_exists = process_exists(descendant_pid);
        if descendant_still_exists {
            // 测试失败时也不得把 sleep 泄漏到后续测试。
            // SAFETY: PID 来自本测试刚启动的后台进程。
            unsafe {
                libc::kill(descendant_pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(&pid_file);

        let error = result.expect_err("background descendant must fail closed");
        assert!(error.contains("后台子进程仍存活"), "{error}");
        assert!(
            elapsed < Duration::from_secs(5),
            "must not wait for the 30-second descendant: {elapsed:?}"
        );
        assert!(!descendant_still_exists, "descendant must be killed");
    }

    #[cfg(unix)]
    #[test]
    fn run_timed_kills_background_descendant_that_holds_output_pipe() {
        assert_background_descendant_is_rejected(
            "(trap '' HUP; sleep 30) & printf '%s' \"$!\" > \"$1\"; exit 0",
            "pipe",
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_timed_kills_background_descendant_even_after_it_closes_output_pipes() {
        assert_background_descendant_is_rejected(
            "(trap '' HUP; sleep 30) >/dev/null 2>&1 & printf '%s' \"$!\" > \"$1\"; exit 0",
            "redirected",
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_run_timed_assigns_job_then_resumes_suspended_child() {
        clear_cancel();
        let mut command = Command::new("cmd.exe");
        command.args(["/D", "/S", "/C", "echo codecli-job-ok"]);
        let output = run_timed(command, 10).expect("run suspended Windows child");
        assert!(output.status_ok);
        assert!(output.stdout.contains("codecli-job-ok"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_run_timed_rejects_background_job_member_with_closed_pipes() {
        clear_cancel();
        let mut command = Command::new("cmd.exe");
        command.args([
            "/D",
            "/S",
            "/C",
            "start \"\" /B cmd.exe /D /S /C \"ping -n 30 127.0.0.1 ^>NUL 2^>^&1\"",
        ]);
        let error =
            run_timed(command, 10).expect_err("background Windows job member must fail closed");
        assert!(
            error.contains("后台子进程仍存活"),
            "unexpected error: {error}"
        );
    }
}
