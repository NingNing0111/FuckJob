use std::{
    fs,
    future::Future,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    pin::Pin,
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, RwLock,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use rust_drission::{stealth_inject, ChromiumPage, Page};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::config::{self, BrowserConfig};

// ================================
// 浏览器环境检测
// ================================

#[derive(Serialize, Debug, Clone)]
pub struct BrowserEnvStatus {
    pub browser_found: bool,
    pub browser_name: Option<String>,
    pub browser_path: Option<String>,
    pub user_data_dir_ok: bool,
    pub user_data_dir: Option<String>,
}

/// 检测系统中已安装的浏览器路径，优先级：Chrome > Edge
pub fn detect_browser_path() -> Option<(&'static str, PathBuf)> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[
            (
                "Chrome",
                &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"],
            ),
            (
                "Edge",
                &["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"],
            ),
        ]
    } else if cfg!(target_os = "windows") {
        &[
            (
                "Chrome",
                &[
                    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
                ],
            ),
            (
                "Edge",
                &[
                    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
                    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                ],
            ),
        ]
    } else {
        &[
            (
                "Chrome",
                &[
                    "/usr/bin/google-chrome",
                    "/usr/bin/google-chrome-stable",
                    "/usr/bin/chromium-browser",
                    "/usr/bin/chromium",
                ],
            ),
            (
                "Edge",
                &["/usr/bin/microsoft-edge", "/usr/bin/microsoft-edge-stable"],
            ),
        ]
    };

    for (name, paths) in candidates {
        for path_str in *paths {
            let path = PathBuf::from(path_str);
            if path.exists() {
                return Some((*name, path));
            }
        }
    }
    None
}

pub fn check_browser_env_status(config: &BrowserConfig) -> BrowserEnvStatus {
    let detected = detect_browser_path();
    let has_explicit_path = config
        .chrome_exe_path
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty() && p.trim() != "null" && p.trim() != "None");

    let (browser_found, browser_name, browser_path) = if has_explicit_path {
        let path = config.chrome_exe_path.as_ref().unwrap();
        (
            PathBuf::from(path).exists(),
            Some("自定义路径".to_string()),
            Some(path.clone()),
        )
    } else if let Some((name, path)) = detected {
        (
            true,
            Some(name.to_string()),
            Some(path.to_string_lossy().to_string()),
        )
    } else {
        (false, None, None)
    };

    let user_data_dir_ok = !config.user_data_dir.trim().is_empty()
        && config.user_data_dir.trim() != "null"
        && config.user_data_dir.trim() != "None";
    let user_data_dir = if user_data_dir_ok {
        Some(config.user_data_dir.clone())
    } else {
        None
    };

    BrowserEnvStatus {
        browser_found,
        browser_name,
        browser_path,
        user_data_dir_ok,
        user_data_dir,
    }
}

static BROWSER_SESSION: Lazy<RwLock<BrowserSession>> =
    Lazy::new(|| RwLock::new(BrowserSession::Empty));
static APP_HANDLE: Lazy<RwLock<Option<tauri::AppHandle>>> = Lazy::new(|| RwLock::new(None));
static ACTIVE_DEBUG_PORT: Lazy<RwLock<Option<u16>>> = Lazy::new(|| RwLock::new(None));
/// Serializes the check-and-launch sequence. `BROWSER_SESSION` also protects its
/// own state, but this lock additionally closes the race between ensuring the
/// managed process and establishing a task-scoped CDP connection.
static MANAGED_BROWSER_START_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static ACTIVE_TASK_BROWSER_LEASES: AtomicUsize = AtomicUsize::new(0);

const BROWSER_START_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedBrowserRecord {
    pid: u32,
    port: u16,
    user_data_dir: String,
}

enum BrowserSession {
    Empty,
    Ready {
        browser: ChromiumPage,
        identity: BrowserSessionIdentity,
    },
    InUse {
        close_requested: bool,
        identity: Option<BrowserSessionIdentity>,
    },
}

/// Tabs created through a task lease. Keeping ownership explicit is important:
/// taking a before/after snapshot of all Chrome tabs is racy when two tasks
/// create tabs at the same time.
struct TaskOwnedTabs<T> {
    tabs: Vec<T>,
}

impl<T> Default for TaskOwnedTabs<T> {
    fn default() -> Self {
        Self { tabs: Vec::new() }
    }
}

impl<T> TaskOwnedTabs<T> {
    fn register(&mut self, tab: T) {
        self.tabs.push(tab);
    }

    fn drain(&mut self) -> Vec<T> {
        std::mem::take(&mut self.tabs)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.tabs.len()
    }
}

/// A task-isolated CDP connection and the tabs created through it.
///
/// The underlying `ChromiumPage` is deliberately only exposed by shared
/// reference. This prevents task code from calling `close_browser()`, while
/// still allowing it to create listeners and use browser-level read APIs.
pub struct TaskBrowserLease {
    browser: ChromiumPage,
    main_tab: Page,
    owned_tabs: Mutex<TaskOwnedTabs<Page>>,
    counted_active: bool,
}

impl TaskBrowserLease {
    /// The task's dedicated initial tab.
    pub fn tab(&self) -> &Page {
        &self.main_tab
    }

    /// The task's independent CDP connection. Only a shared reference is
    /// returned, so this lease cannot terminate the managed Chrome process.
    pub fn browser(&self) -> &ChromiumPage {
        &self.browser
    }

    /// Create another stealth tab owned by this task. It will be closed along
    /// with the main tab when the lease finishes.
    pub fn new_stealth_tab(&self) -> Result<Page> {
        let tab = new_stealth_tab(&self.browser)?;
        self.owned_tabs
            .lock()
            .map_err(|e| anyhow!("获取任务标签页锁失败: {}", e))?
            .register(tab.clone());
        Ok(tab)
    }

    fn close_owned_tabs(&self) -> Result<()> {
        let tabs = self
            .owned_tabs
            .lock()
            .map_err(|e| anyhow!("获取任务标签页锁失败: {}", e))?
            .drain();
        close_task_owned_tabs(tabs)
    }
}

impl Drop for TaskBrowserLease {
    fn drop(&mut self) {
        // Best-effort fallback for cancellation/panic paths. `with_task_browser`
        // performs the same cleanup explicitly so it can report an error.
        let _ = self.close_owned_tabs();
        if self.counted_active {
            ACTIVE_TASK_BROWSER_LEASES.fetch_sub(1, Ordering::AcqRel);
            self.counted_active = false;
        }
    }
}

pub fn init_app_handle(app_handle: tauri::AppHandle) -> Result<()> {
    let mut handle = APP_HANDLE
        .write()
        .map_err(|e| anyhow!("获取应用句柄写锁失败: {}", e))?;
    *handle = Some(app_handle);
    Ok(())
}

pub fn app_handle() -> Option<tauri::AppHandle> {
    APP_HANDLE.read().ok().and_then(|h| h.clone())
}

fn load_browser_config() -> Result<BrowserConfig> {
    let app_handle = APP_HANDLE
        .read()
        .map_err(|e| anyhow!("获取应用句柄读锁失败: {}", e))?
        .clone()
        .ok_or_else(|| anyhow!("应用句柄尚未初始化"))?;

    let config = config::load_app_config_inner(app_handle).map_err(|e| anyhow!(e))?;
    Ok(config.browser_config)
}

/// 初始化浏览器会话
pub fn init_browser_session(config: &BrowserConfig) -> Result<()> {
    let mut session = BROWSER_SESSION
        .write()
        .map_err(|e| anyhow!("获取浏览器会话写锁失败: {}", e))?;

    match &*session {
        BrowserSession::Ready { browser, identity } => {
            if is_browser_session_usable(browser) {
                match ready_session_config_action(
                    identity,
                    config,
                    ACTIVE_TASK_BROWSER_LEASES.load(Ordering::Acquire),
                ) {
                    ReadySessionConfigAction::Reuse => return Ok(()),
                    ReadySessionConfigAction::RejectActiveLeases => {
                        return Err(anyhow!(
                            "浏览器配置已变化，但仍有自动化任务正在使用当前浏览器"
                        ));
                    }
                    ReadySessionConfigAction::Restart => {}
                }
            } else {
                *session = BrowserSession::Empty;
            }
        }
        BrowserSession::InUse { identity, .. } => {
            if identity.as_ref().is_some_and(|identity| {
                ready_session_config_action(
                    identity,
                    config,
                    ACTIVE_TASK_BROWSER_LEASES.load(Ordering::Acquire),
                ) != ReadySessionConfigAction::Reuse
            }) {
                return Err(anyhow!("浏览器正在使用中，无法切换 profile 或调试端口"));
            }
            // A legacy `with_browser` caller currently owns the anchor
            // connection. Task-scoped callers can still attach through CDP;
            // never reset the anchor here because that would race its later
            // restore and could launch/attach a second session unnecessarily.
            let active_port = ACTIVE_DEBUG_PORT
                .read()
                .ok()
                .and_then(|port| *port)
                .is_some_and(is_cdp_port_active);
            if active_port {
                return Ok(());
            }
            return Err(anyhow!("浏览器会话正在初始化或使用中，请稍后重试"));
        }
        BrowserSession::Empty => {}
    }

    let previous_session = std::mem::replace(
        &mut *session,
        BrowserSession::InUse {
            close_requested: false,
            identity: None,
        },
    );
    let previous_session = match previous_session {
        BrowserSession::Ready { mut browser, .. } => {
            browser.close_browser();
            std::thread::sleep(std::time::Duration::from_millis(300));
            BrowserSession::Empty
        }
        previous => previous,
    };

    let executable = browser_executable(config)?.to_string_lossy().into_owned();
    let browser = match create_browser(config) {
        Ok(browser) => browser,
        Err(err) => {
            *session = previous_session;
            return Err(err);
        }
    };
    let debug_port = ACTIVE_DEBUG_PORT
        .read()
        .map_err(|e| anyhow!("获取浏览器调试端口读锁失败: {}", e))?
        .ok_or_else(|| anyhow!("浏览器已初始化但调试端口未知"))?;

    *session = BrowserSession::Ready {
        browser,
        identity: BrowserSessionIdentity {
            user_data_dir: config.user_data_dir.clone(),
            debug_port,
            executable,
        },
    };

    Ok(())
}

fn is_cdp_port_active(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(300)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    if stream
        .write_all(b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return false;
    }
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return false;
    };
    if !headers.starts_with("HTTP/1.1 200") && !headers.starts_with("HTTP/1.0 200") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|value| {
            value
                .get("Browser")
                .and_then(|value| value.as_str())
                .is_some()
                && value
                    .get("webSocketDebuggerUrl")
                    .and_then(|value| value.as_str())
                    .is_some()
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserSessionIdentity {
    user_data_dir: String,
    debug_port: u16,
    executable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadySessionConfigAction {
    Reuse,
    Restart,
    RejectActiveLeases,
}

fn ready_session_config_action_with_executable(
    current: &BrowserSessionIdentity,
    config: &BrowserConfig,
    executable: &str,
    active_leases: usize,
) -> ReadySessionConfigAction {
    let profile_matches = profiles_match(&current.user_data_dir, &config.user_data_dir);
    let port_matches = configured_port_allows_reuse(config, current.debug_port);
    let executable_matches = executable_paths_match(&current.executable, executable);
    if profile_matches && port_matches && executable_matches {
        ReadySessionConfigAction::Reuse
    } else if active_leases > 0 {
        ReadySessionConfigAction::RejectActiveLeases
    } else {
        ReadySessionConfigAction::Restart
    }
}

fn ready_session_config_action(
    current: &BrowserSessionIdentity,
    config: &BrowserConfig,
    active_leases: usize,
) -> ReadySessionConfigAction {
    let executable = browser_executable(config)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    ready_session_config_action_with_executable(current, config, &executable, active_leases)
}

#[cfg(test)]
fn should_connect_to_existing_browser(cdp_port_active: bool) -> bool {
    cdp_port_active
}

fn cdp_connection_requires_stealth_injection() -> bool {
    true
}

#[cfg(test)]
fn managed_browser_can_be_reused(
    record: &ManagedBrowserRecord,
    config: &BrowserConfig,
    cdp_port_active: bool,
) -> bool {
    cdp_port_active && record.user_data_dir == config.user_data_dir
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortOwnership {
    Owned,
    NotOwned,
    Unknown,
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessCommandState {
    Alive {
        browser_process: bool,
        user_data_dir: Option<String>,
    },
    Dead,
    Unknown,
}

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn command_user_data_dir_from_argv(args: &[&[u8]]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let arg = String::from_utf8_lossy(arg);
        if let Some(value) = arg.strip_prefix("--user-data-dir=") {
            return (!value.is_empty()).then(|| value.to_string());
        }
        if arg == "--user-data-dir" {
            return args
                .get(index + 1)
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .filter(|value| !value.is_empty());
        }
    }
    None
}

#[cfg(any(test, target_os = "linux"))]
fn linux_cmdline_state(bytes: Option<&[u8]>) -> ProcessCommandState {
    let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) else {
        return ProcessCommandState::Unknown;
    };
    let args: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect();
    let Some(executable) = args.first() else {
        return ProcessCommandState::Unknown;
    };
    ProcessCommandState::Alive {
        browser_process: command_identifies_browser(&String::from_utf8_lossy(executable)),
        user_data_dir: command_user_data_dir_from_argv(&args),
    }
}

#[cfg(any(test, target_os = "linux"))]
fn linux_cmdline_state_from_result(result: std::io::Result<Vec<u8>>) -> ProcessCommandState {
    match result {
        Ok(bytes) => linux_cmdline_state(Some(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProcessCommandState::Dead,
        Err(_) => ProcessCommandState::Unknown,
    }
}

#[cfg(any(test, target_os = "linux"))]
fn linux_port_ownership_from_evidence(
    fd_complete: bool,
    socket_inodes: &[String],
    tables: &[std::result::Result<String, ()>],
    port: u16,
) -> PortOwnership {
    if !fd_complete
        || socket_inodes.is_empty()
        || tables.len() != 2
        || tables.iter().any(Result::is_err)
    {
        return PortOwnership::Unknown;
    }
    let port_hex = format!("{port:04X}");
    for content in tables.iter().filter_map(|table| table.as_ref().ok()) {
        if !content.is_empty()
            && !content
                .lines()
                .next()
                .is_some_and(|line| line.contains("local_address"))
        {
            return PortOwnership::Unknown;
        }
        for line in content.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() <= 9 || fields[1].rsplit_once(':').is_none() {
                return PortOwnership::Unknown;
            }
            let local_port = fields[1].rsplit_once(':').map(|(_, port)| port);
            if fields[3] == "0A"
                && local_port == Some(port_hex.as_str())
                && socket_inodes.iter().any(|inode| inode == fields[9])
            {
                return PortOwnership::Owned;
            }
        }
    }
    PortOwnership::NotOwned
}

#[cfg(any(test, target_os = "windows"))]
fn windows_port_ownership(command_succeeded: bool, stdout: &str) -> PortOwnership {
    if !command_succeeded || !stdout.lines().any(|line| line == "PORT_QUERY_OK=1") {
        return PortOwnership::Unknown;
    }
    if stdout.lines().any(|line| line == "OWNS=1") {
        PortOwnership::Owned
    } else if stdout.lines().any(|line| line == "OWNS=0") {
        PortOwnership::NotOwned
    } else {
        PortOwnership::Unknown
    }
}

#[cfg(any(test, target_os = "macos"))]
fn macos_command_user_data_dir(command: &str) -> Option<String> {
    let marker = "--user-data-dir";
    let start = command.find(marker)?;
    let remainder = command.get(start + marker.len()..)?;
    if !remainder.starts_with('=') && !remainder.starts_with(char::is_whitespace) {
        return None;
    }
    let value = command_user_data_dir(command)?;
    let quoted = remainder.trim_start().starts_with("=\"")
        || remainder.trim_start().starts_with("='")
        || remainder.trim_start().starts_with('"')
        || remainder.trim_start().starts_with('\'');
    if !quoted && remainder.contains(' ') && command.contains(&format!("{value} ")) {
        None
    } else {
        Some(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildPollAction {
    Exited,
    Continue,
    CleanupAndFail,
}

fn child_poll_action(
    result: &std::io::Result<Option<std::process::ExitStatus>>,
) -> ChildPollAction {
    match result {
        Ok(Some(_)) => ChildPollAction::Exited,
        Ok(None) => ChildPollAction::Continue,
        Err(_) => ChildPollAction::CleanupAndFail,
    }
}

fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let poll_result = child.try_wait();
        match child_poll_action(&poll_result) {
            ChildPollAction::Exited => return child.wait_with_output().map(Some),
            ChildPollAction::Continue => {}
            ChildPollAction::CleanupAndFail => {
                cleanup_spawned_child(&mut child);
                return Err(poll_result.expect_err("poll action must preserve the error"));
            }
        }
        if std::time::Instant::now() >= deadline {
            cleanup_spawned_child(&mut child);
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(test, target_os = "macos"))]
fn parse_macos_procargs(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    if bytes.len() < std::mem::size_of::<i32>() {
        return None;
    }
    let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    let mut cursor = 4;
    while cursor < bytes.len() && bytes[cursor] != 0 {
        cursor += 1;
    }
    while cursor < bytes.len() && bytes[cursor] == 0 {
        cursor += 1;
    }
    let mut args = Vec::with_capacity(argc as usize);
    while cursor < bytes.len() && args.len() < argc as usize {
        let end = bytes[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)?;
        if end > cursor {
            args.push(bytes[cursor..end].to_vec());
        }
        cursor = end + 1;
    }
    (args.len() == argc as usize).then_some(args)
}

#[cfg(any(test, target_os = "macos"))]
fn macos_procargs_state_from_result(result: std::io::Result<Vec<Vec<u8>>>) -> ProcessCommandState {
    match result {
        Ok(args) if !args.is_empty() => {
            let executable = String::from_utf8_lossy(&args[0]);
            let arg_refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            ProcessCommandState::Alive {
                browser_process: command_identifies_browser(&executable),
                user_data_dir: command_user_data_dir_from_argv(&arg_refs),
            }
        }
        Ok(_) => ProcessCommandState::Unknown,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => ProcessCommandState::Dead,
        Err(_) => ProcessCommandState::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn macos_process_argv(pid: u32) -> std::io::Result<Vec<Vec<u8>>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0usize;
    // SAFETY: the first sysctl call only obtains the required output size.
    let first_result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if first_result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: bytes has the capacity returned by the first sysctl call.
    let second_result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if second_result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    bytes.truncate(size);
    parse_macos_procargs(&bytes).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid KERN_PROCARGS2")
    })
}

fn cleanup_spawned_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn run_browser_launch_attempts<T, F>(automatic: bool, mut launch: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let attempts = if automatic { BROWSER_START_ATTEMPTS } else { 1 };
    let mut last_error = None;
    for _ in 0..attempts {
        match launch() {
            Ok(value) => return Ok(value),
            Err(error) if automatic => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Chrome 启动重试次数已耗尽")))
}

#[cfg(any(test, target_os = "macos"))]
fn lsof_port_ownership(exit_code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> PortOwnership {
    match exit_code {
        Some(0) if !stdout.is_empty() => PortOwnership::Owned,
        Some(1) if stdout.is_empty() && stderr.is_empty() => PortOwnership::NotOwned,
        _ => PortOwnership::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedProcessEvidence {
    pid_alive: bool,
    browser_process: bool,
    process_user_data_dir: Option<String>,
    port_ownership: PortOwnership,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedRecordAction {
    NoRecord,
    Reuse(u16),
    RejectPortChange,
    RejectUnverified,
    RemoveStale,
    Ignore,
}

fn command_identifies_browser(command: &str) -> bool {
    let command = command.to_ascii_lowercase();
    command.contains("google chrome")
        || command.contains("chrome.exe")
        || command.contains("chromium")
        || command.contains("microsoft edge")
        || command.contains("msedge.exe")
}

fn command_user_data_dir(command: &str) -> Option<String> {
    let marker = "--user-data-dir";
    let marker_start = command.find(marker)?;
    let argument_quoted = marker_start > 0
        && command
            .as_bytes()
            .get(marker_start - 1)
            .is_some_and(|byte| *byte == b'"');
    let start = marker_start + marker.len();
    let remainder = command[start..].trim_start();
    let remainder = remainder
        .strip_prefix('=')
        .unwrap_or(remainder)
        .trim_start();
    if remainder.is_empty() {
        return None;
    }
    let value = if argument_quoted {
        let end = remainder.find('"')?;
        &remainder[..end]
    } else if let Some(quoted) = remainder.strip_prefix('"') {
        let end = quoted.find('"')?;
        &quoted[..end]
    } else if let Some(quoted) = remainder.strip_prefix('\'') {
        let end = quoted.find('\'')?;
        &quoted[..end]
    } else {
        let end = remainder
            .find(char::is_whitespace)
            .unwrap_or(remainder.len());
        &remainder[..end]
    };
    (!value.is_empty()).then(|| value.to_string())
}

fn normalize_profile_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Some(normalized)
}

fn executable_paths_match(left: &str, right: &str) -> bool {
    profiles_match(left, right)
}

fn profiles_match(left: &str, right: &str) -> bool {
    normalize_profile_path(left)
        .zip(normalize_profile_path(right))
        .is_some_and(|(left, right)| {
            if cfg!(target_os = "windows") {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            } else {
                left == right
            }
        })
}

fn managed_record_action(
    record: Option<&ManagedBrowserRecord>,
    config: &BrowserConfig,
    evidence: &ManagedProcessEvidence,
    cdp_port_active: bool,
) -> ManagedRecordAction {
    let Some(record) = record else {
        return ManagedRecordAction::NoRecord;
    };
    if !evidence.pid_alive {
        return ManagedRecordAction::RemoveStale;
    }
    let Some(process_profile) = evidence.process_user_data_dir.as_deref() else {
        return ManagedRecordAction::RejectUnverified;
    };
    if !evidence.browser_process || !profiles_match(&record.user_data_dir, process_profile) {
        return ManagedRecordAction::RemoveStale;
    }
    if !profiles_match(&config.user_data_dir, process_profile) {
        return ManagedRecordAction::RejectUnverified;
    }
    if !cdp_port_active
        || evidence.port_ownership == PortOwnership::NotOwned
        || evidence.port_ownership == PortOwnership::Unknown
    {
        return ManagedRecordAction::RejectUnverified;
    }
    if !profiles_match(&record.user_data_dir, &config.user_data_dir) {
        return ManagedRecordAction::Ignore;
    }
    if !configured_port_allows_reuse(config, record.port) {
        return ManagedRecordAction::RejectPortChange;
    }
    ManagedRecordAction::Reuse(record.port)
}

fn inspect_managed_process(record: &ManagedBrowserRecord) -> ManagedProcessEvidence {
    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             $p=Get-CimInstance Win32_Process -Filter \"ProcessId={}\"; \
             $connections=@(Get-NetTCPConnection -State Listen -LocalPort {}); \
             Write-Output 'PORT_QUERY_OK=1'; \
             if($p){{Write-Output ('CMD=' + $p.CommandLine)}}; \
             if($connections | Where-Object {{$_.OwningProcess -eq {}}}){{Write-Output 'OWNS=1'}}else{{Write-Output 'OWNS=0'}}",
            record.pid, record.port, record.pid
        );
        let output = command_output_with_timeout(
            Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]),
            Duration::from_secs(3),
        );
        return match output {
            Ok(Some(output)) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let command = stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("CMD="))
                    .unwrap_or_default();
                ManagedProcessEvidence {
                    pid_alive: !command.is_empty(),
                    browser_process: command_identifies_browser(command),
                    process_user_data_dir: command_user_data_dir(command),
                    port_ownership: windows_port_ownership(true, &stdout),
                }
            }
            _ => ManagedProcessEvidence {
                pid_alive: true,
                browser_process: true,
                process_user_data_dir: None,
                port_ownership: PortOwnership::Unknown,
            },
        };
    }

    #[cfg(target_os = "linux")]
    {
        let command_state =
            linux_cmdline_state_from_result(fs::read(format!("/proc/{}/cmdline", record.pid)));
        let (pid_alive, browser_process, process_user_data_dir) = match command_state {
            ProcessCommandState::Alive {
                browser_process,
                user_data_dir,
            } => (true, browser_process, user_data_dir),
            ProcessCommandState::Dead => (false, false, None),
            ProcessCommandState::Unknown => (true, true, None),
        };
        return ManagedProcessEvidence {
            pid_alive,
            browser_process,
            process_user_data_dir,
            port_ownership: linux_process_port_ownership(record.pid, record.port),
        };
    }

    #[cfg(target_os = "macos")]
    {
        let pid = record.pid.to_string();
        let command_state = macos_procargs_state_from_result(macos_process_argv(record.pid));
        let (pid_alive, browser_process, process_user_data_dir) = match command_state {
            ProcessCommandState::Alive {
                browser_process,
                user_data_dir,
            } => (true, browser_process, user_data_dir),
            ProcessCommandState::Dead => (false, false, None),
            ProcessCommandState::Unknown => (true, true, None),
        };
        if !pid_alive || process_user_data_dir.is_none() {
            return ManagedProcessEvidence {
                pid_alive,
                browser_process,
                process_user_data_dir,
                port_ownership: PortOwnership::Unknown,
            };
        }
        let port = format!("TCP:{}", record.port);
        let port_ownership = match command_output_with_timeout(
            Command::new("lsof").args(["-nP", "-a", "-p", &pid, "-i", &port, "-sTCP:LISTEN"]),
            Duration::from_secs(3),
        ) {
            Ok(Some(output)) => {
                lsof_port_ownership(output.status.code(), &output.stdout, &output.stderr)
            }
            _ => PortOwnership::Unknown,
        };
        return ManagedProcessEvidence {
            pid_alive,
            browser_process,
            process_user_data_dir,
            port_ownership,
        };
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    ManagedProcessEvidence {
        pid_alive: false,
        browser_process: false,
        process_user_data_dir: None,
        port_ownership: PortOwnership::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn linux_process_port_ownership(pid: u32, port: u16) -> PortOwnership {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return PortOwnership::Unknown;
    };
    let mut fd_complete = true;
    let mut socket_inodes = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            fd_complete = false;
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            fd_complete = false;
            continue;
        };
        let target = target.to_string_lossy();
        if let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|inode| inode.strip_suffix(']'))
        {
            socket_inodes.push(inode.to_string());
        }
    }
    let tables = ["/proc/net/tcp", "/proc/net/tcp6"]
        .map(|table| fs::read_to_string(table).map_err(|_| ()))
        .to_vec();
    linux_port_ownership_from_evidence(fd_complete, &socket_inodes, &tables, port)
}

fn remove_managed_browser_record() {
    if let Ok(path) = managed_browser_record_path() {
        let _ = fs::remove_file(path);
    }
}

fn configured_port_allows_reuse(config: &BrowserConfig, port: u16) -> bool {
    config
        .debug_port
        .is_none_or(|configured_port| configured_port == 0 || configured_port == port)
}

fn reusable_managed_browser_port(
    record: Option<&ManagedBrowserRecord>,
    config: &BrowserConfig,
) -> Result<Option<u16>> {
    let Some(record) = record else {
        return Ok(None);
    };
    let evidence = inspect_managed_process(record);
    match managed_record_action(
        Some(record),
        config,
        &evidence,
        is_cdp_port_active(record.port),
    ) {
        ManagedRecordAction::Reuse(port) => Ok(Some(port)),
        ManagedRecordAction::RejectPortChange => Err(anyhow!(
            "同一 profile 的受管浏览器仍在端口 {} 运行；请先关闭浏览器后再切换到显式端口",
            record.port
        )),
        ManagedRecordAction::RejectUnverified => Err(anyhow!(
            "无法验证受管浏览器 PID {} 的 profile 或调试端口归属，已拒绝复用或启动第二实例",
            record.pid
        )),
        ManagedRecordAction::RemoveStale => {
            remove_managed_browser_record();
            Ok(None)
        }
        ManagedRecordAction::NoRecord | ManagedRecordAction::Ignore => Ok(None),
    }
}

fn managed_browser_record_path() -> Result<PathBuf> {
    let app_handle = app_handle().ok_or_else(|| anyhow!("应用句柄尚未初始化"))?;
    Ok(app_handle.path().app_data_dir()?.join("rpa-browser.json"))
}

#[derive(Debug, Clone)]
enum ManagedRecordLoad {
    Missing,
    Ready(ManagedBrowserRecord),
    Unverified,
}

fn managed_record_load_from_result(result: std::io::Result<String>) -> ManagedRecordLoad {
    match result {
        Ok(content) => serde_json::from_str(&content)
            .map(ManagedRecordLoad::Ready)
            .unwrap_or(ManagedRecordLoad::Unverified),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ManagedRecordLoad::Missing,
        Err(_) => ManagedRecordLoad::Unverified,
    }
}

fn load_managed_browser_record() -> ManagedRecordLoad {
    let Ok(path) = managed_browser_record_path() else {
        return ManagedRecordLoad::Unverified;
    };
    managed_record_load_from_result(fs::read_to_string(path))
}

fn save_managed_browser_record(record: &ManagedBrowserRecord) -> Result<()> {
    let path = managed_browser_record_path()?;
    let content = serde_json::to_vec(record)?;
    fs::write(path, content)?;
    Ok(())
}

fn browser_executable(config: &BrowserConfig) -> Result<PathBuf> {
    if let Some(path) = config
        .chrome_exe_path
        .as_ref()
        .filter(|path| !path.trim().is_empty())
    {
        return Ok(PathBuf::from(path));
    }
    detect_browser_path()
        .map(|(_, path)| path)
        .ok_or_else(|| anyhow!("未找到可用的 Chrome 或 Edge"))
}

fn connect_to_browser(port: u16) -> Result<ChromiumPage> {
    let endpoint = format!("127.0.0.1:{port}");
    let browser = ChromiumPage::connect(&endpoint)?;
    // ChromiumPage::connect() 不会像 ChromiumPage::new() 一样自动注入。
    // 必须在任何 BOSS 页面导航前注册新文档脚本，避免首个页面加载暴露自动化特征。
    if cdp_connection_requires_stealth_injection() {
        stealth_inject(browser.tab())?;
    }
    if is_browser_session_usable(&browser) {
        Ok(browser)
    } else {
        Err(anyhow!("浏览器调试端口 {port} 的会话不可用"))
    }
}

/// Establish a connection for one task without touching the tab selected by
/// `ChromiumPage::connect`. The caller immediately creates its own tab below.
fn connect_task_browser(port: u16) -> Result<ChromiumPage> {
    let endpoint = format!("127.0.0.1:{port}");
    let browser = ChromiumPage::connect(&endpoint)?;
    if is_browser_session_usable(&browser) {
        Ok(browser)
    } else {
        Err(anyhow!("浏览器调试端口 {port} 的任务连接不可用"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserLaunchAttempt {
    Ready(u16),
    PortClaimed,
    ReadinessTimeout,
}

fn classify_launch_probe_with<F>(cdp_active: bool, port_owned_by_child: F) -> BrowserLaunchAttempt
where
    F: FnOnce() -> bool,
{
    if !cdp_active {
        return BrowserLaunchAttempt::ReadinessTimeout;
    }
    if port_owned_by_child() {
        BrowserLaunchAttempt::Ready(0)
    } else {
        BrowserLaunchAttempt::PortClaimed
    }
}

fn classify_launch_probe(cdp_active: bool, port_owned_by_child: bool) -> BrowserLaunchAttempt {
    classify_launch_probe_with(cdp_active, || port_owned_by_child)
}

fn process_owns_port(pid: u32, port: u16) -> bool {
    inspect_managed_process(&ManagedBrowserRecord {
        pid,
        port,
        user_data_dir: String::new(),
    })
    .port_ownership
        == PortOwnership::Owned
}

fn launch_managed_browser(config: &BrowserConfig, port: u16) -> Result<ChromiumPage> {
    fs::create_dir_all(&config.user_data_dir)?;
    let executable = browser_executable(config)?;
    let mut child = Command::new(executable)
        .args([
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", config.user_data_dir),
            "--window-size=1920,1080".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-suggestions-ui".to_string(),
            "--no-first-run".to_string(),
            "--disable-infobars".to_string(),
            "--disable-popup-blocking".to_string(),
            "--hide-crash-restore-bubble".to_string(),
            "--disable-features=PrivacySandboxSettings4".to_string(),
            "--disable-blink-features=AutomationControlled".to_string(),
            "--no-sandbox".to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..BROWSER_START_ATTEMPTS * 20 {
        let cdp_active = is_cdp_port_active(port);
        match classify_launch_probe_with(cdp_active, || process_owns_port(child.id(), port)) {
            BrowserLaunchAttempt::Ready(_) => {
                if let Ok(browser) = connect_to_browser(port) {
                    let record = ManagedBrowserRecord {
                        pid: child.id(),
                        port,
                        user_data_dir: config.user_data_dir.clone(),
                    };
                    if let Err(error) = save_managed_browser_record(&record) {
                        cleanup_spawned_child(&mut child);
                        return Err(error.context("保存受管浏览器进程记录失败"));
                    }
                    return Ok(browser);
                }
            }
            BrowserLaunchAttempt::PortClaimed => {
                cleanup_spawned_child(&mut child);
                return Err(anyhow!(
                    "Chrome 启动期间调试端口 {port} 被其他进程抢占，已拒绝连接"
                ));
            }
            BrowserLaunchAttempt::ReadinessTimeout => {}
        }
        let poll_result = child.try_wait();
        match child_poll_action(&poll_result) {
            ChildPollAction::Exited => break,
            ChildPollAction::Continue => {}
            ChildPollAction::CleanupAndFail => {
                cleanup_spawned_child(&mut child);
                return Err(poll_result
                    .expect_err("poll action must preserve the error")
                    .into());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    cleanup_spawned_child(&mut child);
    Err(anyhow!("Chrome 启动后未能建立 CDP 连接"))
}

fn create_browser(config: &BrowserConfig) -> Result<ChromiumPage> {
    let managed_record = match load_managed_browser_record() {
        ManagedRecordLoad::Missing => None,
        ManagedRecordLoad::Ready(record) => Some(record),
        ManagedRecordLoad::Unverified => {
            return Err(anyhow!(
                "受管浏览器进程记录无法读取或解析，已拒绝启动第二实例；请确认旧浏览器已关闭后删除 rpa-browser.json"
            ));
        }
    };
    let active_port = ACTIVE_DEBUG_PORT.read().ok().and_then(|guard| *guard);
    let reusable_port = if let Some(port) = active_port.filter(|port| {
        configured_port_allows_reuse(config, *port)
            && managed_record.as_ref().is_some_and(|record| {
                record.port == *port
                    && managed_record_action(
                        Some(record),
                        config,
                        &inspect_managed_process(record),
                        is_cdp_port_active(*port),
                    ) == ManagedRecordAction::Reuse(*port)
            })
    }) {
        Some(port)
    } else {
        reusable_managed_browser_port(managed_record.as_ref(), config)?
    };

    if let Some(port) = reusable_port {
        let browser = connect_to_browser(port)?;
        if let Ok(mut guard) = ACTIVE_DEBUG_PORT.write() {
            *guard = Some(port);
        }
        return Ok(browser);
    }

    // Do not attach to arbitrary listeners or unrelated CDP instances. Only a
    // profile-matching managed record is eligible for reuse.
    let automatic_port = config.debug_port.is_none_or(|port| port == 0);
    let browser = run_browser_launch_attempts(automatic_port, || {
        let target_port = select_browser_debug_port(config)?;
        if TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], target_port)),
            Duration::from_millis(300),
        )
        .is_ok()
        {
            return Err(anyhow!("浏览器调试端口 {target_port} 已被占用"));
        }

        launch_managed_browser(config, target_port).map(|browser| (browser, target_port))
    })?;
    if let Ok(mut guard) = ACTIVE_DEBUG_PORT.write() {
        *guard = Some(browser.1);
    }
    Ok(browser.0)
}

fn select_browser_debug_port(config: &BrowserConfig) -> Result<u16> {
    match config.debug_port {
        Some(port @ 1..=u16::MAX) => Ok(port),
        None | Some(0) => allocate_browser_debug_port(),
    }
}

fn allocate_browser_debug_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| anyhow!("无法分配浏览器调试端口：{error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| anyhow!("无法读取浏览器调试端口：{error}"))?
        .port();
    drop(listener);
    Ok(port)
}

#[cfg(test)]
fn is_browser_start_retryable(error: &str) -> bool {
    error.contains("Chrome did not become ready within 5 seconds after launch")
}

fn is_browser_session_usable(browser: &ChromiumPage) -> bool {
    browser.url().is_ok()
}

pub async fn with_browser<T, F>(f: F) -> Result<T>
where
    F: for<'a> FnOnce(&'a ChromiumPage) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
{
    let config = load_browser_config()?;
    init_browser_session(&config)?;

    let browser = take_browser_session()?;

    let result = f(&browser).await;
    restore_browser_session(browser, result.as_ref().err())?;

    result
}

/// Run one RPA task on an independent CDP connection and an independently
/// owned main tab.
///
/// Calls may run concurrently. Chrome/profile startup is serialized, but the
/// lock is released before the task begins. Cleanup closes only tabs registered
/// to this lease; it never closes Chrome or tabs belonging to another task.
pub async fn with_task_browser<T, F>(op: F) -> Result<T>
where
    F: for<'a> FnOnce(&'a ChromiumPage, &'a Page) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
{
    let config = load_browser_config()?;
    let start_guard = MANAGED_BROWSER_START_LOCK
        .lock()
        .map_err(|e| anyhow!("获取浏览器启动锁失败: {}", e))?;
    let port = ensure_managed_browser_for_task(&config)?;
    // Count the task while still holding the lifecycle lock. This closes the
    // gap in which an explicit browser-close request could otherwise arrive
    // after startup but before the lease became visible.
    ACTIVE_TASK_BROWSER_LEASES.fetch_add(1, Ordering::AcqRel);
    drop(start_guard);

    let browser = match connect_task_browser(port) {
        Ok(browser) => browser,
        Err(error) => {
            ACTIVE_TASK_BROWSER_LEASES.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
    };
    let main_tab = match new_stealth_tab(&browser) {
        Ok(tab) => tab,
        Err(error) => {
            ACTIVE_TASK_BROWSER_LEASES.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
    };
    let mut owned_tabs = TaskOwnedTabs::default();
    owned_tabs.register(main_tab.clone());

    let lease = TaskBrowserLease {
        browser,
        main_tab,
        owned_tabs: Mutex::new(owned_tabs),
        counted_active: true,
    };
    let result = op(lease.browser(), lease.tab()).await;
    // Closing a tab after its renderer/browser has already exited is benign
    // for task cleanup. Never turn an otherwise successful RPA task into a
    // failure solely because best-effort teardown could not close a dead tab.
    let _ = lease.close_owned_tabs();

    result
}

fn ensure_managed_browser_for_task(config: &BrowserConfig) -> Result<u16> {
    // The legacy session owns no task connection. It remains as the process
    // health anchor for environment/login APIs and also serializes launch with
    // callers that have not migrated to task leases yet.
    init_browser_session(config)?;

    let port = ACTIVE_DEBUG_PORT
        .read()
        .map_err(|e| anyhow!("获取浏览器调试端口读锁失败: {}", e))?
        .ok_or_else(|| anyhow!("浏览器已初始化但调试端口未知"))?;
    if !is_cdp_port_active(port) {
        return Err(anyhow!("浏览器调试端口 {port} 不可用"));
    }
    Ok(port)
}

fn close_task_owned_tabs(tabs: Vec<Page>) -> Result<()> {
    close_owned_items(tabs, |tab| tab.close().map_err(anyhow::Error::from))
}

/// Close every owned resource even when one close fails. Returning the first
/// error preserves useful diagnostics without leaking the remaining tabs.
fn close_owned_items<T, F>(items: Vec<T>, mut close: F) -> Result<()>
where
    F: FnMut(T) -> Result<()>,
{
    let mut first_error = None;
    for item in items {
        if let Err(error) = close(item) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn take_browser_session() -> Result<ChromiumPage> {
    let mut session = BROWSER_SESSION
        .write()
        .map_err(|e| anyhow!("获取浏览器会话写锁失败: {}", e))?;

    match std::mem::replace(
        &mut *session,
        BrowserSession::InUse {
            close_requested: false,
            identity: None,
        },
    ) {
        BrowserSession::Ready { browser, identity } => {
            *session = BrowserSession::InUse {
                close_requested: false,
                identity: Some(identity),
            };
            Ok(browser)
        }
        BrowserSession::Empty => {
            *session = BrowserSession::Empty;
            Err(anyhow!("浏览器尚未初始化"))
        }
        BrowserSession::InUse {
            close_requested,
            identity,
        } => {
            *session = BrowserSession::InUse {
                close_requested,
                identity,
            };
            Err(anyhow!("浏览器正在使用中"))
        }
    }
}

fn restore_browser_session(mut browser: ChromiumPage, error: Option<&anyhow::Error>) -> Result<()> {
    let mut session = BROWSER_SESSION
        .write()
        .map_err(|e| anyhow!("获取浏览器会话写锁失败: {}", e))?;

    let (close_requested, identity) = match &*session {
        BrowserSession::InUse {
            close_requested,
            identity,
        } => (*close_requested, identity.clone()),
        _ => (false, None),
    };
    let task_leases_active = ACTIVE_TASK_BROWSER_LEASES.load(Ordering::Acquire) > 0;
    *session = if !task_leases_active && close_requested {
        browser.close_browser();
        BrowserSession::Empty
    } else if error.is_some_and(|err| !reuse_browser_session_after_error(err)) {
        // Discard the broken CDP connection, but do not terminate the managed
        // Chrome process: another task may be between connection setup and
        // lease publication. The next initialization can reconnect by port.
        BrowserSession::Empty
    } else if let Some(identity) = identity {
        BrowserSession::Ready { browser, identity }
    } else {
        BrowserSession::Empty
    };

    Ok(())
}

pub fn close_browser_session() -> Result<()> {
    let _lifecycle_guard = MANAGED_BROWSER_START_LOCK
        .lock()
        .map_err(|e| anyhow!("获取浏览器生命周期锁失败: {}", e))?;
    if ACTIVE_TASK_BROWSER_LEASES.load(Ordering::Acquire) > 0 {
        return Err(anyhow!("仍有自动化任务正在使用浏览器，暂不能关闭"));
    }
    let mut session = BROWSER_SESSION
        .write()
        .map_err(|e| anyhow!("获取浏览器会话写锁失败: {}", e))?;

    match std::mem::replace(&mut *session, BrowserSession::Empty) {
        BrowserSession::Ready { mut browser, .. } => {
            browser.close_browser();
        }
        BrowserSession::InUse { identity, .. } => {
            *session = BrowserSession::InUse {
                close_requested: true,
                identity,
            };
        }
        BrowserSession::Empty => {}
    }

    Ok(())
}

/// 创建一个尚未导航的标签，并在首个文档加载前注册反检测脚本。
///
/// 不使用 `ChromiumPage::new_tab()` 的隐式行为，避免调用方在“网页点击后才
/// 创建的标签”上错过首个页面加载时机。
pub fn new_stealth_tab(browser: &ChromiumPage) -> Result<Page> {
    let tab = browser.new_tab_without_stealth(None)?;
    stealth_inject(&tab)?;
    Ok(tab)
}

// 开启一个tab 执行op后 关闭tab
pub async fn with_new_tab<T, F>(op: F) -> Result<T>
where
    F: for<'a> FnOnce(&'a Page) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
{
    let config = load_browser_config()?;
    init_browser_session(&config)?;
    let tab = {
        let session = BROWSER_SESSION
            .read()
            .map_err(|e| anyhow!("获取浏览器会话读锁失败: {}", e))?;
        let BrowserSession::Ready { browser, .. } = &*session else {
            return Err(anyhow!("浏览器尚未初始化"));
        };
        new_stealth_tab(browser)?
    };

    let result = op(&tab).await;
    let close_result = tab.close();

    match (result, close_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err.into()),
    }
}

fn reuse_browser_session_after_error(error: &anyhow::Error) -> bool {
    !is_browser_disconnected_error(error)
}

fn is_browser_disconnected_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("Connection closed")
        || message.contains("Trying to work with closed connection")
        || message.contains("Session with given id not found")
        || message.contains("code=-32001")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_connection_errors_discard_browser_session() {
        let error = anyhow!("Connection closed");

        assert!(!reuse_browser_session_after_error(&error));
    }

    #[test]
    fn closed_connection_errors_discard_browser_session() {
        let error = anyhow!("Trying to work with closed connection");

        assert!(!reuse_browser_session_after_error(&error));
    }

    #[test]
    fn stale_cdp_session_errors_discard_browser_session() {
        let error = anyhow!(
            "CDP error: id=Some(11), code=-32001, message=Session with given id not found."
        );

        assert!(!reuse_browser_session_after_error(&error));
    }

    #[test]
    fn normal_operation_errors_keep_browser_session() {
        let error = anyhow!("登录失败");

        assert!(reuse_browser_session_after_error(&error));
    }

    #[test]
    fn ready_session_reuses_only_compatible_profile_and_port() {
        let executable = "/test/bin/chrome";
        let current = BrowserSessionIdentity {
            user_data_dir: "/tmp/profile-a".to_string(),
            debug_port: 41000,
            executable: executable.to_string(),
        };
        let compatible_auto = BrowserConfig {
            user_data_dir: current.user_data_dir.clone(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: None,
        };
        let compatible_explicit = BrowserConfig {
            debug_port: Some(current.debug_port),
            ..compatible_auto.clone()
        };
        let different_port = BrowserConfig {
            debug_port: Some(42000),
            ..compatible_auto.clone()
        };
        let different_profile = BrowserConfig {
            user_data_dir: "/tmp/profile-b".to_string(),
            ..compatible_auto.clone()
        };

        assert_eq!(
            ready_session_config_action_with_executable(&current, &compatible_auto, executable, 0,),
            ReadySessionConfigAction::Reuse
        );
        assert_eq!(
            ready_session_config_action_with_executable(
                &current,
                &compatible_explicit,
                executable,
                0,
            ),
            ReadySessionConfigAction::Reuse
        );
        assert_eq!(
            ready_session_config_action_with_executable(&current, &different_port, executable, 0,),
            ReadySessionConfigAction::Restart
        );
        assert_eq!(
            ready_session_config_action_with_executable(
                &current,
                &different_profile,
                executable,
                0,
            ),
            ReadySessionConfigAction::Restart
        );
    }

    #[test]
    fn incompatible_ready_session_is_rejected_while_task_lease_is_active() {
        let executable = "/test/bin/chrome";
        let current = BrowserSessionIdentity {
            user_data_dir: "/tmp/profile-a".to_string(),
            debug_port: 41000,
            executable: executable.to_string(),
        };
        let changed = BrowserConfig {
            user_data_dir: current.user_data_dir.clone(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(42000),
        };

        assert_eq!(
            ready_session_config_action_with_executable(&current, &changed, executable, 1),
            ReadySessionConfigAction::RejectActiveLeases
        );
    }

    #[test]
    fn process_profile_must_match_record_and_config_after_normalization() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/profiles/../profile-a".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(41000),
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 41000,
            user_data_dir: "/tmp/profile-a".to_string(),
        };
        let matching = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: true,
            process_user_data_dir: Some("/tmp/./profile-a".to_string()),
            port_ownership: PortOwnership::Owned,
        };
        let mismatching = ManagedProcessEvidence {
            process_user_data_dir: Some("/tmp/profile-b".to_string()),
            ..matching.clone()
        };
        let unreadable = ManagedProcessEvidence {
            process_user_data_dir: None,
            ..matching.clone()
        };

        assert_eq!(
            managed_record_action(Some(&record), &config, &matching, true),
            ManagedRecordAction::Reuse(41000)
        );
        assert_eq!(
            managed_record_action(Some(&record), &config, &mismatching, true),
            ManagedRecordAction::RemoveStale
        );
        assert_eq!(
            managed_record_action(Some(&record), &config, &unreadable, true),
            ManagedRecordAction::RejectUnverified
        );
    }

    #[test]
    fn parses_user_data_dir_from_browser_command_line() {
        assert_eq!(
            command_user_data_dir(
                r#"Google Chrome --remote-debugging-port=41000 --user-data-dir="/tmp/profile a""#
            ),
            Some("/tmp/profile a".to_string())
        );
        assert_eq!(
            command_user_data_dir(r#"chrome.exe --user-data-dir "C:\Profiles\Offer Flow""#),
            Some(r#"C:\Profiles\Offer Flow"#.to_string())
        );
        assert_eq!(
            command_user_data_dir("chrome --remote-debugging-port=1"),
            None
        );
    }

    #[test]
    fn unknown_port_ownership_rejects_reuse_without_marking_record_stale() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/profile-a".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: None,
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 41000,
            user_data_dir: config.user_data_dir.clone(),
        };
        let evidence = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: true,
            process_user_data_dir: Some(config.user_data_dir.clone()),
            port_ownership: PortOwnership::Unknown,
        };

        assert_eq!(
            managed_record_action(Some(&record), &config, &evidence, true),
            ManagedRecordAction::RejectUnverified
        );
    }

    #[test]
    fn live_same_profile_browser_on_a_different_explicit_port_blocks_launch() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/profile-a".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(42000),
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 41000,
            user_data_dir: config.user_data_dir.clone(),
        };
        let evidence = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: true,
            process_user_data_dir: Some(config.user_data_dir.clone()),
            port_ownership: PortOwnership::Owned,
        };

        assert_eq!(
            managed_record_action(Some(&record), &config, &evidence, true),
            ManagedRecordAction::RejectPortChange
        );
    }

    #[test]
    fn only_dead_or_reused_pid_records_are_stale() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/profile-a".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: None,
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 41000,
            user_data_dir: config.user_data_dir.clone(),
        };
        let dead = ManagedProcessEvidence {
            pid_alive: false,
            browser_process: false,
            process_user_data_dir: None,
            port_ownership: PortOwnership::Unknown,
        };
        let reused_pid = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: false,
            process_user_data_dir: Some("/tmp/unrelated-profile".to_string()),
            port_ownership: PortOwnership::NotOwned,
        };
        let matching_profile_not_owned = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: true,
            process_user_data_dir: Some(config.user_data_dir.clone()),
            port_ownership: PortOwnership::NotOwned,
        };

        assert_eq!(
            managed_record_action(Some(&record), &config, &dead, false),
            ManagedRecordAction::RemoveStale
        );
        assert_eq!(
            managed_record_action(Some(&record), &config, &reused_pid, false),
            ManagedRecordAction::RemoveStale
        );
        assert_eq!(
            managed_record_action(Some(&record), &config, &matching_profile_not_owned, true,),
            ManagedRecordAction::RejectUnverified
        );
    }

    #[test]
    fn linux_argv_preserves_user_data_dir_with_spaces() {
        let args = [
            b"/usr/bin/google-chrome".as_slice(),
            b"--remote-debugging-port=41000".as_slice(),
            b"--user-data-dir".as_slice(),
            b"/home/user/Offer Flow/Profile".as_slice(),
        ];

        assert_eq!(
            command_user_data_dir_from_argv(&args),
            Some("/home/user/Offer Flow/Profile".to_string())
        );
    }

    #[test]
    fn linux_partial_proc_evidence_is_unknown_and_target_port_is_checked() {
        let inode = "12345".to_string();
        let matching = format!(
            "  sl  local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n   0: 0100007F:A028 00000000:0000 0A 0:0 0:0 0 0 0 {}",
            inode
        );
        let non_matching_port = matching.replace("A028", "A029");

        assert_eq!(
            linux_port_ownership_from_evidence(
                false,
                std::slice::from_ref(&inode),
                &[Ok(matching.clone()), Ok(String::new())],
                41000,
            ),
            PortOwnership::Unknown
        );
        assert_eq!(
            linux_port_ownership_from_evidence(
                true,
                std::slice::from_ref(&inode),
                &[Ok(matching.clone()), Err(())],
                41000,
            ),
            PortOwnership::Unknown
        );
        assert_eq!(
            linux_port_ownership_from_evidence(
                true,
                std::slice::from_ref(&inode),
                &[Ok("malformed".to_string()), Ok(String::new())],
                41000,
            ),
            PortOwnership::Unknown
        );
        assert_eq!(
            linux_port_ownership_from_evidence(
                true,
                std::slice::from_ref(&inode),
                &[Ok(matching), Ok(String::new())],
                41000,
            ),
            PortOwnership::Owned
        );
        assert_eq!(
            linux_port_ownership_from_evidence(
                true,
                &[inode],
                &[Ok(non_matching_port), Ok(String::new())],
                41000,
            ),
            PortOwnership::NotOwned
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_spawned_child_kills_and_reaps_the_process() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn test child");

        cleanup_spawned_child(&mut child);

        assert!(
            child.try_wait().expect("read child status").is_some(),
            "cleanup must reap the spawned child"
        );
    }

    #[test]
    fn production_retry_runner_retries_automatic_but_not_explicit_ports() {
        let mut automatic_calls = 0;
        let automatic = run_browser_launch_attempts(true, || {
            automatic_calls += 1;
            if automatic_calls < 3 {
                Err(anyhow!("not ready"))
            } else {
                Ok(43000)
            }
        });
        assert_eq!(automatic.unwrap(), 43000);
        assert_eq!(automatic_calls, 3);

        let mut explicit_calls = 0;
        let explicit: Result<u16> = run_browser_launch_attempts(false, || {
            explicit_calls += 1;
            Err(anyhow!("not ready"))
        });
        assert!(explicit.is_err());
        assert_eq!(explicit_calls, 1);
    }

    #[test]
    fn macos_ambiguous_unquoted_profile_with_spaces_is_unknown() {
        assert_eq!(
            macos_command_user_data_dir(
                "Google Chrome --user-data-dir=/Users/test/Offer Flow/Profile --remote-debugging-port=41000"
            ),
            None
        );
    }

    #[test]
    fn windows_missing_explicit_ownership_result_is_unknown() {
        assert_eq!(
            windows_port_ownership(true, "CMD=chrome.exe --user-data-dir=C:\\profile"),
            PortOwnership::Unknown
        );
        assert_eq!(
            windows_port_ownership(true, "PORT_QUERY_OK=1\nOWNS=0"),
            PortOwnership::NotOwned
        );
    }

    #[test]
    fn linux_proc_not_found_is_dead_but_other_failures_are_unknown() {
        assert_eq!(
            linux_cmdline_state_from_result(Err(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            ))),
            ProcessCommandState::Dead
        );
        assert_eq!(
            linux_cmdline_state_from_result(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            ProcessCommandState::Unknown
        );
        assert_eq!(
            linux_cmdline_state_from_result(Ok(Vec::new())),
            ProcessCommandState::Unknown
        );
    }

    #[test]
    fn linux_empty_or_unreadable_proc_evidence_is_unknown() {
        assert_eq!(
            linux_port_ownership_from_evidence(false, &[], &[], 41000),
            PortOwnership::Unknown
        );
        assert_eq!(linux_cmdline_state(None), ProcessCommandState::Unknown);
        assert_eq!(linux_cmdline_state(Some(&[])), ProcessCommandState::Unknown);
    }

    #[test]
    fn macos_lsof_exit_one_requires_clean_empty_output_for_not_owned() {
        assert_eq!(
            lsof_port_ownership(Some(1), b"", b""),
            PortOwnership::NotOwned
        );
        assert_eq!(
            lsof_port_ownership(Some(1), b"", b"permission denied"),
            PortOwnership::Unknown
        );
        assert_eq!(
            lsof_port_ownership(None, b"", b"tool unavailable"),
            PortOwnership::Unknown
        );
    }

    #[test]
    fn live_process_with_matching_profile_and_unreachable_cdp_is_unverified() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/profile-a".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(41000),
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 41000,
            user_data_dir: config.user_data_dir.clone(),
        };
        let evidence = ManagedProcessEvidence {
            pid_alive: true,
            browser_process: true,
            process_user_data_dir: Some(config.user_data_dir.clone()),
            port_ownership: PortOwnership::Owned,
        };

        assert_eq!(
            managed_record_action(Some(&record), &config, &evidence, true),
            ManagedRecordAction::Reuse(41000)
        );
        assert_eq!(
            managed_record_action(Some(&record), &config, &evidence, false),
            ManagedRecordAction::RejectUnverified
        );
    }

    #[test]
    fn parses_only_browser_process_identity() {
        assert!(command_identifies_browser(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome --remote-debugging-port=41000"
        ));
        assert!(command_identifies_browser(
            "msedge.exe --remote-debugging-port=41000"
        ));
        assert!(!command_identifies_browser("node unrelated-cdp-server.js"));
    }

    #[test]
    fn inactive_cdp_skips_expensive_port_ownership_probe() {
        let mut calls = 0;
        let result = classify_launch_probe_with(false, || {
            calls += 1;
            true
        });

        assert_eq!(result, BrowserLaunchAttempt::ReadinessTimeout);
        assert_eq!(calls, 0);
    }

    #[cfg(unix)]
    #[test]
    fn external_process_probe_times_out_and_reaps_child() {
        let started = std::time::Instant::now();
        let output = command_output_with_timeout(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(50),
        )
        .expect("run bounded command");

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn macos_missing_process_is_dead_but_probe_failures_are_unknown() {
        assert_eq!(
            macos_procargs_state_from_result(Err(std::io::Error::from_raw_os_error(libc::ESRCH))),
            ProcessCommandState::Dead
        );
        assert_eq!(
            macos_procargs_state_from_result(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            ProcessCommandState::Unknown
        );
        assert_eq!(
            macos_procargs_state_from_result(Ok(Vec::new())),
            ProcessCommandState::Unknown
        );
    }

    #[test]
    fn macos_procargs_parser_preserves_profile_paths_with_spaces() {
        let mut bytes = (4_i32).to_ne_bytes().to_vec();
        bytes
            .extend_from_slice(b"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\0\0");
        bytes.extend_from_slice(b"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\0");
        bytes.extend_from_slice(b"--remote-debugging-port=41000\0");
        bytes.extend_from_slice(
            b"--user-data-dir=/Users/test/Library/Application Support/Offer Flow/Profile\0",
        );
        bytes.extend_from_slice(b"--no-first-run\0");

        let args = parse_macos_procargs(&bytes).expect("parse process argv");
        let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        assert_eq!(
            command_user_data_dir_from_argv(&refs),
            Some("/Users/test/Library/Application Support/Offer Flow/Profile".to_string())
        );
    }

    #[test]
    fn windows_whole_quoted_user_data_dir_argument_preserves_spaces() {
        assert_eq!(
            command_user_data_dir(
                r#"chrome.exe "--user-data-dir=C:\Users\test\Application Support\Offer Flow\Profile" --remote-debugging-port=41000"#,
            ),
            Some(r"C:\Users\test\Application Support\Offer Flow\Profile".to_string())
        );
    }

    #[test]
    fn launch_wait_refuses_cdp_not_owned_by_spawned_pid() {
        assert_eq!(
            classify_launch_probe(true, false),
            BrowserLaunchAttempt::PortClaimed
        );
        assert_eq!(
            classify_launch_probe(true, true),
            BrowserLaunchAttempt::Ready(0)
        );
    }

    #[test]
    fn configured_debug_port_uses_explicit_value_and_zero_allocates_dynamically() {
        let explicit = BrowserConfig {
            user_data_dir: "/tmp/offer-flow-profile".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(43210),
        };
        assert_eq!(select_browser_debug_port(&explicit).unwrap(), 43210);

        let automatic = BrowserConfig {
            debug_port: Some(0),
            ..explicit
        };
        let port = select_browser_debug_port(&automatic).unwrap();
        assert_ne!(port, 0);
    }

    #[test]
    fn non_cdp_listener_is_not_treated_as_a_managed_browser() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nOK",
                )
                .unwrap();
        });

        assert!(!is_cdp_port_active(port));
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn process_probe_wait_errors_require_cleanup() {
        assert_eq!(
            child_poll_action(&Err(std::io::Error::other("probe failed"))),
            ChildPollAction::CleanupAndFail
        );
    }

    #[test]
    fn executable_changes_invalidate_ready_session_identity() {
        let current = BrowserSessionIdentity {
            user_data_dir: "/tmp/profile".to_string(),
            debug_port: 41000,
            executable: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
        };
        let config = BrowserConfig {
            user_data_dir: current.user_data_dir.clone(),
            chrome_exe_path: Some(
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".to_string(),
            ),
            max_parallel_tasks: 2,
            debug_port: Some(41000),
        };
        assert_eq!(
            ready_session_config_action_with_executable(
                &current,
                &config,
                &config.chrome_exe_path.clone().unwrap(),
                0,
            ),
            ReadySessionConfigAction::Restart
        );
    }

    #[test]
    fn missing_record_allows_launch_but_corrupt_record_fails_closed() {
        assert!(matches!(
            managed_record_load_from_result(Err(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            ))),
            ManagedRecordLoad::Missing
        ));
        assert!(matches!(
            managed_record_load_from_result(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied,
            ))),
            ManagedRecordLoad::Unverified
        ));
        assert!(matches!(
            managed_record_load_from_result(Ok("not json".to_string())),
            ManagedRecordLoad::Unverified
        ));
    }

    #[test]
    fn recorded_managed_browser_is_reused_only_when_cdp_and_profile_match() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            let body = r#"{"Browser":"Chrome/120","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/browser/test"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let config = BrowserConfig {
            user_data_dir: "/tmp/offer-flow-profile".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: None,
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port,
            user_data_dir: config.user_data_dir.clone(),
        };

        let error = reusable_managed_browser_port(Some(&record), &config)
            .expect_err("unverifiable managed records must fail closed");
        assert!(error.to_string().contains("无法验证受管浏览器"));
        server.join().unwrap();
    }

    #[test]
    fn explicit_debug_port_does_not_reuse_a_recorded_different_port() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        use std::io::{Read, Write};
                        let mut request = [0_u8; 512];
                        let _ = stream.read(&mut request);
                        let body = r#"{"Browser":"Chrome/120","webSocketDebuggerUrl":"ws://127.0.0.1/devtools/browser/test"}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        stream.write_all(response.as_bytes()).unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept CDP test connection: {error}"),
                }
            }
        });
        let config = BrowserConfig {
            user_data_dir: "/tmp/offer-flow-profile".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(43210),
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port,
            user_data_dir: config.user_data_dir.clone(),
        };

        assert_eq!(
            managed_record_action(
                Some(&record),
                &config,
                &ManagedProcessEvidence {
                    pid_alive: true,
                    browser_process: true,
                    process_user_data_dir: Some(config.user_data_dir.clone()),
                    port_ownership: PortOwnership::Owned,
                },
                true,
            ),
            ManagedRecordAction::RejectPortChange
        );
        server.join().unwrap();
    }

    #[test]
    fn explicit_debug_port_rejects_a_different_active_port() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/offer-flow-profile".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: Some(43210),
        };

        assert!(!configured_port_allows_reuse(&config, 50000));
        assert!(configured_port_allows_reuse(&config, 43210));
    }

    #[test]
    fn allocates_a_nonzero_local_debug_port() {
        let port = allocate_browser_debug_port().expect("allocate browser debug port");

        assert_ne!(port, 0);
    }

    #[test]
    fn retries_only_browser_readiness_timeout() {
        assert!(is_browser_start_retryable(
            "HTTP request failed: Chrome did not become ready within 5 seconds after launch"
        ));
        assert!(!is_browser_start_retryable(
            "Failed to launch Chrome: No such file or directory"
        ));
    }

    #[test]
    fn active_cdp_port_uses_connection_instead_of_launching_a_second_browser() {
        assert!(should_connect_to_existing_browser(true));
        assert!(!should_connect_to_existing_browser(false));
    }

    #[test]
    fn live_managed_browser_is_reused_even_when_no_page_is_open() {
        let config = BrowserConfig {
            user_data_dir: "/tmp/offer-flow-profile".to_string(),
            chrome_exe_path: None,
            max_parallel_tasks: 2,
            debug_port: None,
        };
        let record = ManagedBrowserRecord {
            pid: 123,
            port: 43210,
            user_data_dir: config.user_data_dir.clone(),
        };

        assert!(managed_browser_can_be_reused(&record, &config, true));
        assert!(!managed_browser_can_be_reused(&record, &config, false));
    }

    #[test]
    fn cdp_connections_install_stealth_before_the_next_navigation() {
        assert!(cdp_connection_requires_stealth_injection());
    }

    #[test]
    fn task_owned_tabs_track_only_explicitly_registered_tabs() {
        let mut owned = TaskOwnedTabs::default();
        owned.register("task-main");
        owned.register("task-child");

        assert_eq!(owned.len(), 2);
        assert_eq!(owned.drain(), vec!["task-main", "task-child"]);
        assert_eq!(owned.len(), 0);
    }

    #[test]
    fn task_cleanup_attempts_every_owned_tab_and_reports_first_error() {
        let mut closed = Vec::new();
        let result = close_owned_items(vec![1, 2, 3], |tab_id| {
            closed.push(tab_id);
            if tab_id == 2 {
                Err(anyhow!("tab 2 close failed"))
            } else {
                Ok(())
            }
        });

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "tab 2 close failed");
        assert_eq!(closed, vec![1, 2, 3]);
    }

    #[test]
    fn task_cleanup_with_no_owned_tabs_is_a_noop() {
        let mut close_called = false;
        let result = close_owned_items(Vec::<u8>::new(), |_| {
            close_called = true;
            Ok(())
        });

        assert!(result.is_ok());
        assert!(!close_called);
    }
}
