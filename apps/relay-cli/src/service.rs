#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use relay_engine::Engine;

pub const LAUNCH_AGENT_LABEL: &str = "dev.relay.agent";
pub const WINDOWS_TASK_NAME: &str = "Relay";
const UNSUPPORTED: &str = "relay service is supported on macOS and Windows; on Linux run `relay run --log-file ...` under systemd";
const NOT_INITIALIZED: &str = "this device is not initialized; run: relay init --name <name>";
const LISTEN_IN_USE: &str = "is the background service running? stop it with: relay service stop";

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    /// Register and start the background service (also upgrades an existing install)
    Install {
        #[arg(long, default_value = "0.0.0.0:47321")]
        listen: SocketAddr,
    },
    /// Stop and unregister the service (keeps the binary, data, and logs)
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
    /// Print the service log
    Logs {
        #[arg(short = 'n', default_value_t = 50)]
        n: usize,
        #[arg(short = 'f', long)]
        follow: bool,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub binary: String,
    pub log: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

pub fn supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

pub fn listen_in_use_hint() -> &'static str {
    LISTEN_IN_USE
}

pub fn is_addr_in_use(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::AddrInUse
        {
            return true;
        }
        let text = cause.to_string();
        if text.to_ascii_lowercase().contains("already in use")
            || text.to_ascii_lowercase().contains("address in use")
        {
            return true;
        }
    }
    false
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("relay.log")
}

pub fn run(home: &Path, cmd: ServiceCmd, json: bool) -> Result<ExitCode> {
    if let ServiceCmd::Logs { n, follow } = cmd {
        return cmd_logs(home, n, follow);
    }
    require_supported()?;
    match cmd {
        ServiceCmd::Install { listen } => {
            require_initialized(home)?;
            install(home, listen)?;
            print_install_details(home, listen, json)?;
            print_status(&status_info(home)?, json)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Uninstall => {
            uninstall(home)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Start => {
            start(home)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Stop => {
            stop(home)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Restart => {
            restart(home)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Status => {
            print_status(&status_info(home)?, json)?;
            Ok(ExitCode::SUCCESS)
        }
        ServiceCmd::Logs { .. } => cmd_logs(home, 50, false),
    }
}

pub fn format_status_line(status: &ServiceStatus) -> String {
    if status.running {
        match status.pid {
            Some(pid) => format!("service: running (pid {pid})"),
            None => "service: running".to_owned(),
        }
    } else if status.installed {
        "service: installed, not running".to_owned()
    } else {
        "service: not installed".to_owned()
    }
}

pub fn format_service_status_human(status: &ServiceStatus) -> String {
    let installed = if status.installed { "yes" } else { "no" };
    let running = if status.running {
        match status.pid {
            Some(pid) => format!("yes (pid {pid})"),
            None => "yes".to_owned(),
        }
    } else {
        "no".to_owned()
    };
    let mut out = format!(
        "installed: {installed}\nrunning: {running}\nbinary: {}\nlog: {}",
        status.binary, status.log
    );
    if let Some(state) = &status.state {
        out.push_str(&format!("\nstate: {state}"));
    }
    out
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn launch_agent_plist(binary: &str, home: &str, listen: &str, log: &str) -> String {
    let launchd_log = format!("{}/logs/launchd.log", home.trim_end_matches('/'));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>--home</string>
		<string>{home}</string>
		<string>run</string>
		<string>--listen</string>
		<string>{listen}</string>
		<string>--log-file</string>
		<string>{log}</string>
		<string>--host</string>
		<string>service</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>StandardOutPath</key>
	<string>{launchd}</string>
	<key>StandardErrorPath</key>
	<string>{launchd}</string>
</dict>
</plist>
"#,
        label = xml_escape(LAUNCH_AGENT_LABEL),
        binary = xml_escape(binary),
        home = xml_escape(home),
        listen = xml_escape(listen),
        log = xml_escape(log),
        launchd = xml_escape(&launchd_log),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchctlParsed {
    pub running: bool,
    pub pid: Option<u32>,
}

pub fn parse_launchctl_print(output: &str) -> LaunchctlParsed {
    let mut running = false;
    let mut pid = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("state = ") {
            running = value.trim() == "running";
        }
        if let Some(value) = line.strip_prefix("pid = ") {
            pid = value
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|n| *n > 0);
        }
    }
    LaunchctlParsed { running, pid }
}

pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn windows_task_argument_string(home: &str, listen: &str, log: &str) -> String {
    format!("--home \"{home}\" run --listen {listen} --log-file \"{log}\" --host service")
}

pub fn windows_install_exe_path(localappdata: &str) -> PathBuf {
    PathBuf::from(localappdata)
        .join("Programs")
        .join("Relay")
        .join("relay.exe")
}

pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(s) if s.starts_with(r"\\?\") => PathBuf::from(&s[r"\\?\".len()..]),
        _ => path.to_path_buf(),
    }
}

pub fn encode_powershell_command(script: &str) -> String {
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    base64_encode(&bytes)
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i];
        let b1 = if i + 1 < input.len() { input[i + 1] } else { 0 };
        let b2 = if i + 2 < input.len() { input[i + 2] } else { 0 };
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if i + 1 < input.len() {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < input.len() {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

pub const PS_STOP_KILL: &str = r#"if (Get-ScheduledTask -TaskName Relay -ErrorAction SilentlyContinue) {
  Stop-ScheduledTask -TaskName Relay -ErrorAction SilentlyContinue
}
$deadline = (Get-Date).AddSeconds(15)
while ((Get-Date) -lt $deadline) {
  $procs = Get-Process relay -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }
  if (-not $procs) { break }
  Start-Sleep -Milliseconds 250
}
Get-Process relay -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
"#;

pub const WINDOWS_INSTALL_BODY: &str = r#"$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) {
  throw 'relay service install needs an elevated (Administrator) terminal. SSH sessions of administrator accounts are elevated; locally, right-click Terminal > Run as administrator.'
}
"#;

pub const WINDOWS_INSTALL_AFTER_STOP: &str = r#"if ($src.ToLowerInvariant() -ne $exe.ToLowerInvariant()) {
  $dir = Split-Path -Parent $exe
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $copied = $false
  foreach ($i in 1..20) {
    try {
      Copy-Item -LiteralPath $src -Destination $exe -Force
      $copied = $true
      break
    } catch {
      Start-Sleep -Milliseconds 500
    }
  }
  if (-not $copied) {
    throw "failed to copy relay.exe to $exe"
  }
  Unblock-File -LiteralPath $exe -ErrorAction SilentlyContinue
}
$dir = Split-Path -Parent $exe
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @()
if ($null -ne $userPath -and $userPath -ne '') {
  $parts = $userPath -split ';'
}
$found = $false
foreach ($p in $parts) {
  if ($p -and ($p.TrimEnd('\') -ieq $dir.TrimEnd('\'))) {
    $found = $true
    break
  }
}
if (-not $found) {
  if ($null -eq $userPath -or $userPath -eq '') {
    [Environment]::SetEnvironmentVariable('Path', $dir, 'User')
  } else {
    $sep = ''
    if (-not $userPath.EndsWith(';')) { $sep = ';' }
    [Environment]::SetEnvironmentVariable('Path', ($userPath + $sep + $dir), 'User')
  }
}
$action = New-ScheduledTaskAction -Execute $exe -Argument $taskArgs
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName Relay -Force -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
Get-NetFirewallRule -DisplayName 'Relay' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'Relay' -Direction Inbound -Program $exe -Protocol UDP -LocalPort $port -Action Allow -Profile Private,Domain | Out-Null
Get-NetConnectionProfile | ForEach-Object {
  if ($_.NetworkCategory -eq 'Public') {
    $n = $_.Name
    Write-Output ("warning=the network {0} is Public so Windows blocks Relay; fix with Set-NetConnectionProfile -Name '{0}' -NetworkCategory Private" -f $n)
  }
}
Start-ScheduledTask -TaskName Relay
"#;

pub fn windows_install_script(src: &str, exe: &str, port: u16, task_args: &str) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(&format!("$src = {}\n", ps_quote(src)));
    script.push_str(&format!("$exe = {}\n", ps_quote(exe)));
    script.push_str(&format!("$taskArgs = {}\n", ps_quote(task_args)));
    script.push_str(&format!("$port = {port}\n"));
    script.push_str(WINDOWS_INSTALL_BODY);
    script.push_str(PS_STOP_KILL);
    script.push_str(WINDOWS_INSTALL_AFTER_STOP);
    script
}

pub fn windows_stop_script(exe: &str) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(&format!("$exe = {}\n", ps_quote(exe)));
    script.push_str(PS_STOP_KILL);
    script
}

pub fn windows_start_script() -> String {
    String::from("$ErrorActionPreference = 'Stop'\nStart-ScheduledTask -TaskName Relay\n")
}

pub fn windows_restart_script(exe: &str) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(&format!("$exe = {}\n", ps_quote(exe)));
    script.push_str(PS_STOP_KILL);
    script.push_str("Start-ScheduledTask -TaskName Relay\n");
    script
}

pub fn windows_uninstall_script(exe: &str) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(&format!("$exe = {}\n", ps_quote(exe)));
    script.push_str(PS_STOP_KILL);
    script.push_str(
        r#"if (Get-ScheduledTask -TaskName Relay -ErrorAction SilentlyContinue) {
  Unregister-ScheduledTask -TaskName Relay -Confirm:$false
}
Get-NetFirewallRule -DisplayName 'Relay' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
"#,
    );
    script
}

pub fn windows_status_script(exe: &str) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(&format!("$exe = {}\n", ps_quote(exe)));
    script.push_str(
        r#"$task = Get-ScheduledTask -TaskName Relay -ErrorAction SilentlyContinue
if ($task) {
  Write-Output 'installed=yes'
  Write-Output ('state=' + [string]$task.State)
} else {
  Write-Output 'installed=no'
}
$proc = @(Get-Process relay -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }) | Select-Object -First 1
if ($proc) {
  Write-Output 'running=yes'
  Write-Output ('pid=' + $proc.Id)
} else {
  Write-Output 'running=no'
}
"#,
    );
    script
}

pub fn parse_key_values(stdout: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=')
            && key != "warning"
        {
            map.insert(key.to_owned(), value.to_owned());
        }
    }
    map
}

pub fn last_n_lines(contents: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

fn require_supported() -> Result<()> {
    if supported() {
        Ok(())
    } else {
        bail!("{UNSUPPORTED}")
    }
}

fn require_initialized(home: &Path) -> Result<()> {
    if Engine::open_read_only(home).is_ok() {
        Ok(())
    } else {
        bail!("{NOT_INITIALIZED}")
    }
}

fn print_install_details(home: &Path, listen: SocketAddr, json: bool) -> Result<()> {
    if json {
        return Ok(());
    }
    let binary = current_binary_display()?;
    println!("binary: {binary}");
    println!("home: {}", home.display());
    println!("log: {}", log_path(home).display());
    println!("listen: {listen}");
    Ok(())
}

fn print_status(status: &ServiceStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        println!("{}", format_service_status_human(status));
    }
    Ok(())
}

fn cmd_logs(home: &Path, n: usize, follow: bool) -> Result<ExitCode> {
    let path = log_path(home);
    if path.exists() {
        for line in tail_lines(&path, n).with_context(|| format!("reading {}", path.display()))? {
            println!("{line}");
        }
    }
    if !follow {
        return Ok(ExitCode::SUCCESS);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
    })
    .context("installing Ctrl-C handler")?;

    let mut pos = file_len(&path).unwrap_or(0);
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(500));
        match follow_once(&path, &mut pos) {
            Ok(chunk) => {
                if !chunk.is_empty() {
                    print!("{chunk}");
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                pos = 0;
            }
            Err(_) => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn file_len(path: &Path) -> std::io::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

/// The last `n` lines, read backwards in chunks so a large log is not
/// loaded whole.
fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    const CHUNK: u64 = 64 * 1024;
    let mut file = File::open(path)?;
    let mut start = file.metadata()?.len();
    let mut buf: Vec<u8> = Vec::new();
    while start > 0 {
        let from = start.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (start - from) as usize];
        file.seek(SeekFrom::Start(from))?;
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        start = from;
        // More newlines than lines wanted means the first (possibly cut)
        // line in the buffer is not one of them.
        if bytecount_newlines(&buf) > n {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(last_n_lines(&text, n)
        .into_iter()
        .map(str::to_owned)
        .collect())
}

fn bytecount_newlines(buf: &[u8]) -> usize {
    buf.iter().filter(|b| **b == b'\n').count()
}
fn follow_once(path: &Path, pos: &mut u64) -> std::io::Result<String> {
    let len = fs::metadata(path)?.len();
    if len < *pos {
        *pos = 0;
    }
    if len == *pos {
        return Ok(String::new());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*pos))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    *pos += buf.len() as u64;
    Ok(buf)
}

fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current executable path")?;
    let canonical = exe.canonicalize().unwrap_or(exe);
    Ok(strip_verbatim_prefix(&canonical))
}

fn current_binary_display() -> Result<String> {
    #[cfg(windows)]
    let path = windows_install_exe()?;
    #[cfg(not(windows))]
    let path = current_binary()?;
    Ok(path.display().to_string())
}

#[cfg(any(target_os = "macos", windows))]
fn ensure_logs_dir(home: &Path) -> Result<PathBuf> {
    let dir = home.join("logs");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(log_path(home))
}

#[cfg(target_os = "macos")]
fn install(home: &Path, listen: SocketAddr) -> Result<()> {
    macos::install(home, listen)
}

#[cfg(windows)]
fn install(home: &Path, listen: SocketAddr) -> Result<()> {
    windows::install(home, listen)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install(_home: &Path, _listen: SocketAddr) -> Result<()> {
    bail!("{UNSUPPORTED}")
}

#[cfg(target_os = "macos")]
fn uninstall(home: &Path) -> Result<()> {
    macos::uninstall(home)
}

#[cfg(windows)]
fn uninstall(home: &Path) -> Result<()> {
    windows::uninstall(home)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn uninstall(_home: &Path) -> Result<()> {
    bail!("{UNSUPPORTED}")
}

#[cfg(target_os = "macos")]
fn start(home: &Path) -> Result<()> {
    macos::start(home)
}

#[cfg(windows)]
fn start(home: &Path) -> Result<()> {
    windows::start(home)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn start(_home: &Path) -> Result<()> {
    bail!("{UNSUPPORTED}")
}

#[cfg(target_os = "macos")]
fn stop(home: &Path) -> Result<()> {
    macos::stop(home)
}

#[cfg(windows)]
fn stop(home: &Path) -> Result<()> {
    windows::stop(home)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn stop(_home: &Path) -> Result<()> {
    bail!("{UNSUPPORTED}")
}

#[cfg(target_os = "macos")]
fn restart(home: &Path) -> Result<()> {
    macos::restart(home)
}

#[cfg(windows)]
fn restart(home: &Path) -> Result<()> {
    windows::restart(home)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn restart(_home: &Path) -> Result<()> {
    bail!("{UNSUPPORTED}")
}

pub fn status_info(home: &Path) -> Result<ServiceStatus> {
    #[cfg(target_os = "macos")]
    {
        macos::status_info(home)
    }
    #[cfg(windows)]
    {
        windows::status_info(home)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = home;
        bail!("{UNSUPPORTED}")
    }
}

#[cfg(any(target_os = "macos", windows))]
fn run_os_command(program: &str, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program} {}", args.join(" ")))
}

#[cfg(any(target_os = "macos", windows))]
fn command_text(output: &std::process::Output) -> String {
    format!(
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub fn install(home: &Path, listen: SocketAddr) -> Result<()> {
        let log = ensure_logs_dir(home)?;
        let binary = current_binary()?;
        let plist_path = launch_agent_plist_path()?;
        if let Some(parent) = plist_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let xml = launch_agent_plist(
            &binary.display().to_string(),
            &home.display().to_string(),
            &listen.to_string(),
            &log.display().to_string(),
        );
        fs::write(&plist_path, xml).with_context(|| format!("writing {}", plist_path.display()))?;

        let uid = macos_uid()?;
        let _ = launchctl_bootout(&uid);
        launchctl_bootstrap(&uid, &plist_path)?;
        Ok(())
    }

    pub fn uninstall(_home: &Path) -> Result<()> {
        let uid = macos_uid()?;
        launchctl_bootout(&uid)?;
        let plist_path = launch_agent_plist_path()?;
        if plist_path.exists() {
            fs::remove_file(&plist_path)
                .with_context(|| format!("removing {}", plist_path.display()))?;
        }
        Ok(())
    }

    pub fn start(_home: &Path) -> Result<()> {
        let plist_path = launch_agent_plist_path()?;
        if !plist_path.exists() {
            bail!("Relay service is not installed; run: relay service install");
        }
        let uid = macos_uid()?;
        launchctl_bootstrap(&uid, &plist_path)
    }

    pub fn stop(_home: &Path) -> Result<()> {
        let uid = macos_uid()?;
        launchctl_bootout(&uid)
    }

    pub fn restart(home: &Path) -> Result<()> {
        let uid = macos_uid()?;
        let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        let output = run_os_command("launchctl", &["kickstart", "-k", &target])?;
        if output.status.success() {
            return Ok(());
        }
        let text = command_text(&output);
        if looks_unloaded(&text) {
            return start(home);
        }
        bail!("launchctl kickstart -k {target} failed: {text}");
    }

    pub fn status_info(home: &Path) -> Result<ServiceStatus> {
        let plist_path = launch_agent_plist_path()?;
        let installed = plist_path.exists();
        let uid = macos_uid()?;
        let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        let output = run_os_command("launchctl", &["print", &target])?;
        let parsed = if output.status.success() {
            parse_launchctl_print(&String::from_utf8_lossy(&output.stdout))
        } else {
            LaunchctlParsed {
                running: false,
                pid: None,
            }
        };
        Ok(ServiceStatus {
            installed,
            running: parsed.running,
            pid: parsed.pid,
            binary: current_binary()?.display().to_string(),
            log: log_path(home).display().to_string(),
            state: None,
        })
    }

    fn launch_agent_plist_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCH_AGENT_LABEL}.plist")))
    }

    fn macos_uid() -> Result<String> {
        let output = run_os_command("id", &["-u"])?;
        if !output.status.success() {
            bail!("id -u failed: {}", command_text(&output));
        }
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if uid.is_empty() {
            bail!("id -u returned an empty uid");
        }
        Ok(uid)
    }

    fn launchctl_bootstrap(uid: &str, plist: &Path) -> Result<()> {
        let domain = format!("gui/{uid}");
        let plist_s = plist.to_string_lossy();
        let mut last = String::new();
        for attempt in 0..10 {
            let output = run_os_command("launchctl", &["bootstrap", &domain, plist_s.as_ref()])?;
            if output.status.success() {
                return Ok(());
            }
            last = command_text(&output);
            if looks_already_loaded(&last) {
                return Ok(());
            }
            if attempt + 1 < 10 {
                thread::sleep(Duration::from_millis(500));
            }
        }
        if last.contains("domain/125") {
            bail!(
                "no GUI login session for this user; log in on the Mac once (the agent runs in your login session)"
            );
        }
        bail!(
            "launchctl bootstrap {domain} {} failed after retries: {last}",
            plist.display()
        );
    }

    fn launchctl_bootout(uid: &str) -> Result<()> {
        let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        let output = run_os_command("launchctl", &["bootout", &target])?;
        if output.status.success() {
            return Ok(());
        }
        let text = command_text(&output);
        if looks_unloaded(&text) {
            return Ok(());
        }
        bail!("launchctl bootout {target} failed: {text}");
    }

    fn looks_unloaded(text: &str) -> bool {
        let lower = text.to_ascii_lowercase();
        lower.contains("no such process")
            || lower.contains("not loaded")
            || lower.contains("could not find")
            || lower.contains("no such service")
    }

    fn looks_already_loaded(text: &str) -> bool {
        let lower = text.to_ascii_lowercase();
        lower.contains("already") && (lower.contains("loaded") || lower.contains("exist"))
    }
}

#[cfg(windows)]
fn windows_install_exe() -> Result<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(windows_install_exe_path(&local))
}

#[cfg(windows)]
mod windows {
    use super::*;

    pub fn install(home: &Path, listen: SocketAddr) -> Result<()> {
        let log = ensure_logs_dir(home)?;
        let src = current_binary()?;
        let exe = windows_install_exe()?;
        let src_s = path_utf8(&src)?;
        let exe_s = path_utf8(&exe)?;
        let home_s = path_utf8(home)?;
        let log_s = path_utf8(&log)?;
        let listen_s = listen.to_string();
        let task_args = windows_task_argument_string(&home_s, &listen_s, &log_s);
        let script = windows_install_script(&src_s, &exe_s, listen.port(), &task_args);
        let stdout = run_powershell(&script)?;
        emit_warnings(&stdout);
        Ok(())
    }

    pub fn uninstall(_home: &Path) -> Result<()> {
        let exe = windows_install_exe()?;
        let script = windows_uninstall_script(&path_utf8(&exe)?);
        let stdout = run_powershell(&script)?;
        emit_warnings(&stdout);
        Ok(())
    }

    pub fn start(_home: &Path) -> Result<()> {
        let stdout = run_powershell(&windows_start_script())?;
        emit_warnings(&stdout);
        Ok(())
    }

    pub fn stop(_home: &Path) -> Result<()> {
        let exe = windows_install_exe()?;
        let script = windows_stop_script(&path_utf8(&exe)?);
        let stdout = run_powershell(&script)?;
        emit_warnings(&stdout);
        Ok(())
    }

    pub fn restart(_home: &Path) -> Result<()> {
        let exe = windows_install_exe()?;
        let script = windows_restart_script(&path_utf8(&exe)?);
        let stdout = run_powershell(&script)?;
        emit_warnings(&stdout);
        Ok(())
    }

    pub fn status_info(home: &Path) -> Result<ServiceStatus> {
        let exe = windows_install_exe()?;
        let exe_s = path_utf8(&exe)?;
        let stdout = run_powershell(&windows_status_script(&exe_s))?;
        emit_warnings(&stdout);
        let kv = parse_key_values(&stdout);
        let installed = kv.get("installed").map(|s| s == "yes").unwrap_or(false);
        let running = kv.get("running").map(|s| s == "yes").unwrap_or(false);
        let pid = kv
            .get("pid")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0);
        let state = kv.get("state").cloned().filter(|s| !s.is_empty());
        Ok(ServiceStatus {
            installed,
            running,
            pid,
            binary: exe_s,
            log: log_path(home).display().to_string(),
            state,
        })
    }

    fn path_utf8(path: &Path) -> Result<String> {
        path.to_str()
            .map(ToOwned::to_owned)
            .with_context(|| format!("path is not UTF-8: {}", path.display()))
    }

    fn emit_warnings(stdout: &str) {
        for line in stdout.lines() {
            if let Some(msg) = line.strip_prefix("warning=") {
                eprintln!("warning: {msg}");
            }
        }
    }

    fn run_powershell(script: &str) -> Result<String> {
        let encoded = encode_powershell_command(script);
        let output = run_os_command(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &encoded,
            ],
        )?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            bail!(
                "powershell.exe exited {}: stdout: {stdout} stderr: {stderr}",
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_owned())
            );
        }
        Ok(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_and_orders_arguments() {
        let xml = launch_agent_plist(
            r#"/opt/bin/re<lay>"#,
            r#"/Users/a&b/Relay"#,
            "0.0.0.0:47321",
            r#"/Users/a&b/Relay/logs/relay.log"#,
        );
        assert!(xml.contains("<string>dev.relay.agent</string>"));
        assert!(xml.contains("<string>/opt/bin/re&lt;lay&gt;</string>"));
        assert!(xml.contains("<string>/Users/a&amp;b/Relay</string>"));
        assert!(xml.contains("<string>/Users/a&amp;b/Relay/logs/relay.log</string>"));
        assert!(xml.contains("<string>/Users/a&amp;b/Relay/logs/launchd.log</string>"));
        assert!(!xml.contains("ProcessType"));
        let args = xml
            .split("<key>ProgramArguments</key>")
            .nth(1)
            .unwrap()
            .split("</array>")
            .next()
            .unwrap();
        let strings: Vec<_> = args
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("<string>")
                    .and_then(|s| s.strip_suffix("</string>"))
            })
            .collect();
        assert_eq!(
            strings,
            [
                "/opt/bin/re&lt;lay&gt;",
                "--home",
                "/Users/a&amp;b/Relay",
                "run",
                "--listen",
                "0.0.0.0:47321",
                "--log-file",
                "/Users/a&amp;b/Relay/logs/relay.log",
                "--host",
                "service",
            ]
        );
    }

    #[test]
    fn plist_escapes_quotes() {
        let xml = launch_agent_plist(r#"C:\a"b'c"#, "home", "127.0.0.1:1", "log");
        assert!(xml.contains("C:\\a&quot;b&apos;c"));
    }

    #[test]
    fn parse_launchctl_running_with_pid() {
        let out = r#"
gui/501/dev.relay.agent = {
	state = running
	program = /Users/me/.cargo/bin/relay
	pid = 43210
}
"#;
        assert_eq!(
            parse_launchctl_print(out),
            LaunchctlParsed {
                running: true,
                pid: Some(43210)
            }
        );
    }

    #[test]
    fn parse_launchctl_not_running() {
        let out = r#"
	state = waiting
	pid = 0
"#;
        assert_eq!(
            parse_launchctl_print(out),
            LaunchctlParsed {
                running: false,
                pid: None
            }
        );
    }

    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("plain"), "'plain'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("a'b'c"), "'a''b''c'");
    }

    #[test]
    fn windows_task_args_quote_paths() {
        assert_eq!(
            windows_task_argument_string(
                r"C:\Users\me\AppData\Roaming\Relay",
                "0.0.0.0:47321",
                r"C:\Users\me\AppData\Roaming\Relay\logs\relay.log",
            ),
            r#"--home "C:\Users\me\AppData\Roaming\Relay" run --listen 0.0.0.0:47321 --log-file "C:\Users\me\AppData\Roaming\Relay\logs\relay.log" --host service"#
        );
    }

    #[test]
    fn install_script_quotes_values_and_sets_stop() {
        let script = windows_install_script(
            r"C:\src\relay.exe",
            r"C:\Users\me\AppData\Local\Programs\Relay\relay.exe",
            47321,
            r#"--home "D:\data" run --listen 0.0.0.0:47321 --log-file "D:\data\logs\relay.log" --host service"#,
        );
        assert!(script.starts_with("$ErrorActionPreference = 'Stop'\n"));
        assert!(script.contains("$src = 'C:\\src\\relay.exe'"));
        assert!(script.contains("$port = 47321"));
        assert!(script.contains("Register-ScheduledTask -TaskName Relay -Force"));
        assert!(script.contains("-LogonType S4U"));
        assert!(script.contains("-LocalPort $port"));
        assert!(!script.contains("C:\\src\\relay.exe\n"));
    }

    #[test]
    fn base64_and_utf16le_encode_known_value() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
        // UTF-16LE of "A" is 41 00
        assert_eq!(encode_powershell_command("A"), "QQA=");
    }

    #[test]
    fn strip_verbatim_prefix_drops_extended_path() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\Relay\relay.exe")),
            PathBuf::from(r"C:\Relay\relay.exe")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new("/usr/bin/relay")),
            PathBuf::from("/usr/bin/relay")
        );
    }

    #[test]
    fn last_n_lines_takes_tail() {
        assert_eq!(last_n_lines("a\nb\nc\n", 2), ["b", "c"]);
        assert_eq!(last_n_lines("only\n", 50), ["only"]);
    }

    #[test]
    fn tail_lines_reads_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.log");
        // Longer than one chunk, so the tail spans a chunk boundary.
        let mut body = String::new();
        for i in 1..=5000 {
            body.push_str(&format!("line-{i} {}\n", "x".repeat(40)));
        }
        fs::write(&path, &body).unwrap();
        let tail = tail_lines(&path, 3).unwrap();
        assert_eq!(tail.len(), 3);
        assert!(tail[0].starts_with("line-4998 "), "{tail:?}");
        assert!(tail[2].starts_with("line-5000 "), "{tail:?}");

        fs::write(&path, "a\nb\nc").unwrap();
        assert_eq!(tail_lines(&path, 2).unwrap(), ["b", "c"]);
        assert_eq!(tail_lines(&path, 50).unwrap(), ["a", "b", "c"]);
        fs::write(&path, "").unwrap();
        assert!(tail_lines(&path, 5).unwrap().is_empty());
    }

    #[test]
    fn follow_reopens_when_length_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.log");
        fs::write(&path, "abcdef\n").unwrap();
        let mut pos = 7;
        fs::write(&path, "xy\n").unwrap();
        let chunk = follow_once(&path, &mut pos).unwrap();
        assert_eq!(chunk, "xy\n");
        assert_eq!(pos, 3);
    }

    #[test]
    fn status_line_variants() {
        let base = ServiceStatus {
            installed: false,
            running: false,
            pid: None,
            binary: "b".into(),
            log: "l".into(),
            state: None,
        };
        assert_eq!(format_status_line(&base), "service: not installed");
        assert_eq!(
            format_status_line(&ServiceStatus {
                installed: true,
                ..base.clone()
            }),
            "service: installed, not running"
        );
        assert_eq!(
            format_status_line(&ServiceStatus {
                installed: true,
                running: true,
                pid: Some(9),
                ..base
            }),
            "service: running (pid 9)"
        );
    }
}
