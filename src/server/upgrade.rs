// Copyright (c) 2026 chulingera2025
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Zero-downtime binary upgrade orchestration (`raddy upgrade`, M7, ADR-008).
//!
//! Pingora's graceful-upgrade mechanism hands the running instance's listening
//! file descriptors to a replacement process over a Unix socket: the
//! replacement (`raddy run -u`) binds the upgrade socket and waits; on SIGQUIT
//! the old process sends its fds over the socket, waits a short takeover
//! window, then drains in-flight requests and exits.
//!
//! `raddy upgrade` is the operator-facing driver — it is executed with the
//! **new** binary, and orchestrates the whole dance: locate the running
//! instance (pidfile), pre-flight the new binary against the same config
//! (`raddy run -t`, which validates and exits before binding anything), spawn
//! the replacement in `-u` mode, wait for it to be listening on the upgrade
//! socket, then SIGQUIT the old process. Any failure aborts before the running
//! instance is disturbed.

use crate::config::ast::CompiledConfig;
use crate::config::snapshot;
use crate::layer4::udp::UdpProxy;
use crate::server::startup::RunOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long the replacement has to bind the upgrade socket.
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the old process to hand off, drain, and exit. It takes
/// ~5s takeover window + the 10s grace period we configure, so 60s is ample.
const OLD_PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Brief pause after the socket appears so the replacement is fully listening
/// before the old process is asked to connect. The old side retries connecting
/// for several seconds, so this is only a best-effort de-race.
const SOCKET_STABILIZE: Duration = Duration::from_millis(500);

/// Execute the zero-downtime upgrade against the running instance found in the
/// pidfile.
///
/// On success the replacement is serving on the inherited listeners and the old
/// process has exited; `raddy upgrade` itself then returns.
pub fn upgrade(config_path: &Path, opts: &RunOptions) -> Result<(), String> {
    let pidfile = opts
        .pidfile
        .as_ref()
        .ok_or("--pidfile is required to locate the running raddy instance")?;
    let old_pid = read_pid(pidfile)?;
    if !process_alive(old_pid) || !is_raddy_process(old_pid) {
        return Err(format!(
            "no running raddy instance: pidfile {} refers to pid {old_pid}, which is not a running raddy process",
            pidfile.display()
        ));
    }
    let snapshot = snapshot::build(config_path)
        .map_err(|e| format!("cannot inspect config before upgrade: {e}"))?;
    let topology_file = topology_path(pidfile);
    let expected_topology = topology_signature(&snapshot);
    let actual_topology = std::fs::read_to_string(&topology_file).map_err(|e| {
        format!(
            "cannot verify listener topology from {}: {e}; use a normal restart",
            topology_file.display()
        )
    })?;
    if actual_topology.trim() != expected_topology {
        return Err(
            "current config listener topology differs from the running instance; use a normal restart"
                .to_string(),
        );
    }
    if snapshot.layer4.iter().any(|listener| {
        matches!(
            listener,
            crate::config::ast::Layer4Listener::Tcp(tcp) if tcp.transparent
        )
    }) {
        return Err(
            "transparent TCP listeners are not compatible with zero-downtime upgrade; use a restart"
                .to_string(),
        );
    }

    let exe =
        std::env::current_exe().map_err(|e| format!("cannot resolve own executable path: {e}"))?;

    // Pre-flight: the *new* binary must boot against the same config (and
    // construct the entire server) before the running instance is disturbed.
    eprintln!("raddy: pre-flight check of the new binary");
    let status = Command::new(&exe)
        .args(server_args("run", &["-t"], config_path, opts))
        .status()
        .map_err(|e| format!("failed to run pre-flight check: {e}"))?;
    if !status.success() {
        return Err(format!(
            "pre-flight check failed (exit {:?}); aborting upgrade — the running instance is untouched",
            status.code()
        ));
    }

    // Drop any stale socket from a crashed upgrade so the readiness probe below
    // only ever observes the replacement's fresh socket.
    let _ = std::fs::remove_file(&opts.upgrade_sock);
    cleanup_udp_handoff_files(&opts.upgrade_sock);

    eprintln!(
        "raddy: spawning replacement {} (waiting on {})",
        exe.display(),
        opts.upgrade_sock
    );
    // The replacement is spawned detached and outlives `raddy upgrade`; it
    // takes over the running instance's listeners.
    let _ = Command::new(&exe)
        .args(server_args("run", &["-u"], config_path, opts))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn replacement raddy: {e}"))?;

    // The replacement binds the upgrade socket during bootstrap and then waits
    // for fds; signal the old process only once it is listening.
    wait_for_socket(&opts.upgrade_sock, SOCKET_WAIT_TIMEOUT)?;
    thread::sleep(SOCKET_STABILIZE);

    eprintln!("raddy: signaling running instance (pid {old_pid}) with SIGQUIT");
    signal_quit(old_pid)?;

    wait_for_exit(old_pid, OLD_PROCESS_EXIT_TIMEOUT)?;

    // Confirm the replacement actually took over: the pidfile must now name a
    // different, live process. Without this check a race where the old process
    // died on its own at the signal moment could report a false success while
    // the replacement never received the listeners.
    let new_pid = read_pid(pidfile)?;
    if new_pid == old_pid || !process_alive(new_pid) {
        return Err(format!(
            "replacement did not take over the listeners (pidfile still names pid {new_pid}); \
             check the replacement's logs"
        ));
    }
    eprintln!("raddy: upgrade complete (now pid {new_pid})");
    Ok(())
}

/// The sidecar file that records the immutable listener topology of a running
/// process identified by pidfile.
pub(crate) fn topology_path(pidfile: &Path) -> PathBuf {
    PathBuf::from(format!("{}.topology", pidfile.display()))
}

/// Compute a stable digest of HTTP, TLS, layer-4, and ACME listener topology.
pub(crate) fn topology_signature(config: &CompiledConfig) -> String {
    let mut input = String::new();
    for key in crate::server::startup::http_listener_topology_keys(config) {
        input.push_str(&key);
        input.push('\n');
    }
    for key in crate::server::startup::l4_listener_topology_keys(config) {
        input.push_str(&key);
        input.push('\n');
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Persist the listener-topology digest next to the pidfile.
pub(crate) fn write_topology_state(pidfile: &Path, config: &CompiledConfig) -> Result<(), String> {
    let path = topology_path(pidfile);
    std::fs::write(&path, topology_signature(config))
        .map_err(|e| format!("failed to write topology state {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to protect topology state {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Remove exact UDP handoff artifacts from a previous interrupted upgrade.
fn cleanup_udp_handoff_files(upgrade_sock: &str) {
    let manifest = UdpProxy::handoff_manifest_path(upgrade_sock);
    let _ = std::fs::remove_file(&manifest);
    let Some(parent) = Path::new(upgrade_sock).parent() else {
        return;
    };
    let Some(prefix) = Path::new(upgrade_sock).file_name() else {
        return;
    };
    let prefix = format!("{}.udp.", prefix.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Build the `raddy run` argument list for a sub-invocation of this same binary
/// (`flags` carries the mode flag, `-t` for the pre-flight or `-u` for the
/// replacement). Paths are absolutized so the child resolves them identically
/// regardless of the current working directory.
fn server_args(sub: &str, flags: &[&str], config_path: &Path, opts: &RunOptions) -> Vec<String> {
    let mut args = vec![sub.to_string()];
    args.extend(flags.iter().map(|f| f.to_string()));
    args.push("-c".to_string());
    args.push(absolutize(config_path).display().to_string());
    args.push("--cert-dir".to_string());
    args.push(absolutize(&opts.cert_dir).display().to_string());
    args.push("--acme-directory".to_string());
    args.push(opts.acme_directory.clone());
    if let Some(root) = &opts.acme_root_pem {
        args.push("--acme-root-pem".to_string());
        args.push(absolutize(root).display().to_string());
    }
    if let Some(log) = &opts.access_log {
        args.push("--access-log".to_string());
        args.push(absolutize(log).display().to_string());
    }
    if let Some(metrics) = &opts.metrics_addr {
        args.push("--metrics-addr".to_string());
        args.push(metrics.clone());
    }
    if let Some(pidfile) = &opts.pidfile {
        args.push("--pidfile".to_string());
        args.push(absolutize(pidfile).display().to_string());
    }
    args.push("--upgrade-sock".to_string());
    args.push(opts.upgrade_sock.clone());
    args
}

/// Resolve `path` against the current directory when it is relative.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Read a PID from a pidfile.
fn read_pid(path: &Path) -> Result<i32, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read pidfile {}: {e}", path.display()))?;
    contents
        .trim()
        .parse::<i32>()
        .map_err(|e| format!("pidfile {} has an invalid pid: {e}", path.display()))
}

/// Whether a process exists and is not (yet) a zombie.
///
/// A zombie still answers `kill(pid, 0)` — it lingers in the process table until
/// its parent reaps it — so on Linux the state is read from `/proc/<pid>/stat`
/// instead, and a zombie counts as exited (the fd handoff is done; only the
/// parent's reap is pending).
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Format: "pid (comm) state ..."; `comm` may contain spaces or parens,
        // so the state is the first field after the last `)`.
        let Some(after_comm) = stat.rfind(')') else {
            return false;
        };
        let state = stat[after_comm + 1..]
            .split_whitespace()
            .next()
            .unwrap_or("?");
        state != "Z"
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 performs no signal; it only checks existence.
        let ret = unsafe { libc::kill(pid, 0) };
        ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Whether a process is a raddy instance, checked by its process name so an
/// upgrade never SIGQUITs an unrelated process that a stale or reused PID may
/// point at.
fn is_raddy_process(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|comm| comm.trim() == "raddy")
        .unwrap_or(false)
}

/// Send SIGQUIT — pingora's graceful-upgrade signal (ADR-008) — to `pid`.
fn signal_quit(pid: i32) -> Result<(), String> {
    // SAFETY: `pid` is the running raddy instance we verified alive.
    if unsafe { libc::kill(pid, libc::SIGQUIT) } != 0 {
        Err(format!(
            "failed to signal SIGQUIT to pid {pid}: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

/// Wait (bounded) for `pid` to exit, which confirms the fd handoff completed and
/// the old process drained.
fn wait_for_exit(pid: i32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "running instance (pid {pid}) did not exit within {timeout:?}; the replacement is serving — check its logs"
    ))
}

/// Wait (bounded) for the upgrade socket file to appear (the replacement is
/// listening for fds).
fn wait_for_socket(sock: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if Path::new(sock).exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "replacement did not bind upgrade socket {sock} within {timeout:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_raddy_process_rejects_non_raddy() {
        // The test binary is not named `raddy`, so the guard must refuse to
        // signal it (this is the "don't SIGQUIT an unrelated process" check).
        assert!(!is_raddy_process(std::process::id() as i32));
        assert!(!is_raddy_process(-1));
    }
}
