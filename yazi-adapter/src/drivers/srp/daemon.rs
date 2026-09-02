//! Minimal port of `som_srv::daemon`'s `connect_or_spawn` — finds and, if
//! necessary, spawns the shared `som-srv` daemon this driver talks to.
//!
//! Unlike Som/`somcat` (which look for `som-srv` next to their OWN
//! executable — see `som_srv::daemon::binary_path_next_to_current_exe`'s
//! doc comment — because they're built and deployed together as one
//! release), this driver has no such relationship to `som-srv`: `ya`/
//! `yazi` is a completely separate project/install, with nothing
//! guaranteeing a `som-srv` binary sits anywhere near its own executable.
//! Instead this always looks in the SAME fixed, well-known location
//! `somcat`'s own error message already points users at when its
//! next-to-exe lookup fails (`~/.local/bin/som-srv[.exe]`) — a user who's
//! set up Som's rich-content transport at all already has a binary there.

use super::pipe::PipeConnection;
use super::protocol::daemon_socket_path;
use std::path::PathBuf;
use std::time::Duration;

const CONNECT_RETRY_ATTEMPTS: u32 = 20;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// `~/.local/bin/som-srv` (`.exe` on Windows) — the one fixed location
/// this driver ever looks in. `None` if `$HOME`/`%USERPROFILE%` can't be
/// determined at all (no sensible fallback in that case).
fn daemon_binary_path() -> Option<PathBuf> {
	let home = dirs_home()?;
	let name = if cfg!(target_os = "windows") { "som-srv.exe" } else { "som-srv" };
	Some(home.join(".local").join("bin").join(name))
}

#[cfg(windows)]
fn dirs_home() -> Option<PathBuf> {
	std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(unix)]
fn dirs_home() -> Option<PathBuf> {
	std::env::var_os("HOME").map(PathBuf::from)
}

/// Tries to connect to the daemon's fixed socket first; if nothing's
/// listening, spawns a detached `~/.local/bin/som-srv --daemon` and
/// retries until it comes up (or gives up). Returns the raw, untagged
/// connection — the caller still writes `ConnectionKind::Srv` before
/// anything else, same as `som_srv::daemon::connect_or_spawn`'s own
/// contract.
pub fn connect_or_spawn() -> anyhow::Result<PipeConnection> {
	let socket_path = daemon_socket_path();

	if let Ok(connection) = PipeConnection::connect(&socket_path) {
		return Ok(connection);
	}

	let daemon_binary_path = daemon_binary_path()
		.ok_or_else(|| anyhow::anyhow!("could not determine home directory to locate ~/.local/bin/som-srv"))?;
	if !daemon_binary_path.is_file() {
		anyhow::bail!(
			"som-srv daemon not running and no binary found at {daemon_binary_path:?} \
			 (install Som's som-srv there to enable SRP rich content in yazi)"
		);
	}
	spawn_detached(&daemon_binary_path)?;

	for attempt in 0..CONNECT_RETRY_ATTEMPTS {
		match PipeConnection::connect(&socket_path) {
			Ok(connection) => return Ok(connection),
			Err(_) if attempt + 1 < CONNECT_RETRY_ATTEMPTS => std::thread::sleep(CONNECT_RETRY_DELAY),
			Err(err) => return Err(err),
		}
	}
	anyhow::bail!("could not connect to som-srv daemon at {socket_path:?} after spawning {daemon_binary_path:?}")
}

#[cfg(target_os = "windows")]
fn spawn_detached(daemon_binary_path: &std::path::Path) -> anyhow::Result<()> {
	use std::os::windows::process::CommandExt;

	const DETACHED_PROCESS: u32 = 0x0000_0008;
	const CREATE_NO_WINDOW: u32 = 0x0800_0000;

	std::process::Command::new(daemon_binary_path)
		.arg("--daemon")
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
		.spawn()?;
	Ok(())
}

#[cfg(unix)]
fn spawn_detached(daemon_binary_path: &std::path::Path) -> anyhow::Result<()> {
	use std::os::unix::process::CommandExt;

	let mut command = std::process::Command::new(daemon_binary_path);
	command.arg("--daemon");
	unsafe {
		// SAFETY: `setsid()` is async-signal-safe and the only thing done
		// here between fork and exec — no allocation, no locking.
		command.pre_exec(|| {
			if libc::setsid() == -1 {
				return Err(std::io::Error::last_os_error());
			}
			Ok(())
		});
	}
	command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
	Ok(())
}
