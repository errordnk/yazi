//! Minimal CLIENT-only port of `som_srv::pipe` — connects to an already-
//! running `som-srv` daemon and speaks its length-prefixed framing. Unlike
//! the original (which also implements the daemon's own accept-a-
//! connection side, `bind`/`accept_on`), this driver never listens for
//! anything — it only ever dials out — so everything server-side is
//! dropped. See `som_srv::pipe`'s own module doc comment for why Windows
//! needs overlapped I/O here at all (a synchronous named pipe handle
//! would serialize concurrent reads/writes from different threads on the
//! same handle) — that constraint is unchanged for a client connection,
//! since `srv_channel::SrvChannel` still has one thread reading
//! (`read_incoming`, waiting for a forwarded `RequestByteRange`) while
//! another writes (`put_chunk`) concurrently.

#[cfg(windows)]
mod windows_impl {
	use anyhow::{Result, bail};
	use std::ffi::OsStr;
	use std::io;
	use std::os::windows::ffi::OsStrExt;
	use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
	use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
	use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
	use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
	use windows::core::PCWSTR;

	fn to_wide(s: &str) -> Vec<u16> { OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect() }

	struct Overlapped {
		inner: OVERLAPPED,
		event: HANDLE,
	}

	impl Overlapped {
		fn new() -> Result<Self> {
			let event = unsafe { CreateEventW(None, true, false, None) }?;
			let mut inner = OVERLAPPED::default();
			inner.hEvent = event;
			Ok(Self { inner, event })
		}
	}

	impl Drop for Overlapped {
		fn drop(&mut self) {
			unsafe {
				CloseHandle(self.event).ok();
			}
		}
	}

	pub struct PipeConnection {
		handle: HANDLE,
	}

	// SAFETY: same reasoning as `som_srv::pipe::windows`'s own
	// `PipeConnection` — every overlapped ReadFile/WriteFile call carries
	// its own OVERLAPPED (and thus its own completion event), so
	// concurrent calls from different threads on this HANDLE don't share
	// mutable state.
	unsafe impl Send for PipeConnection {}
	unsafe impl Sync for PipeConnection {}

	impl PipeConnection {
		pub fn connect(pipe_name: &str) -> Result<Self> {
			let wide_name = to_wide(pipe_name);
			let handle = unsafe {
				windows::Win32::Storage::FileSystem::CreateFileW(
					PCWSTR(wide_name.as_ptr()),
					(GENERIC_READ | GENERIC_WRITE).0,
					windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
					None,
					windows::Win32::Storage::FileSystem::OPEN_EXISTING,
					windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED,
					None,
				)
			}?;
			if handle == INVALID_HANDLE_VALUE {
				bail!("CreateFileW failed: {}", io::Error::last_os_error());
			}
			Ok(Self { handle })
		}

		pub fn read_message(&self) -> Result<Vec<u8>> {
			let mut len_bytes = [0u8; 4];
			self.read_exact(&mut len_bytes)?;
			let len = u32::from_le_bytes(len_bytes) as usize;
			let mut buf = vec![0u8; len];
			self.read_exact(&mut buf)?;
			Ok(buf)
		}

		pub fn write_message(&self, payload: &[u8]) -> Result<()> {
			let len = (payload.len() as u32).to_le_bytes();
			self.write_all(&len)?;
			self.write_all(payload)?;
			Ok(())
		}

		fn read_exact(&self, mut buf: &mut [u8]) -> Result<()> {
			while !buf.is_empty() {
				let overlapped = Overlapped::new()?;
				let mut read = 0u32;
				let ok = unsafe { ReadFile(self.handle, Some(buf), Some(&mut read), Some(&overlapped.inner as *const _ as *mut _)) };
				if ok.is_err() {
					let err = unsafe { windows::Win32::Foundation::GetLastError() };
					if err == ERROR_IO_PENDING {
						unsafe { WaitForSingleObject(overlapped.event, INFINITE) };
						let mut actually_read = 0u32;
						let completed = unsafe { GetOverlappedResult(self.handle, &overlapped.inner, &mut actually_read, false) };
						if completed.is_err() || actually_read == 0 {
							bail!("pipe closed or overlapped ReadFile failed");
						}
						read = actually_read;
					} else {
						bail!("pipe closed or ReadFile failed: {err:?}");
					}
				}
				if read == 0 {
					bail!("pipe closed (zero-byte read)");
				}
				buf = &mut buf[read as usize..];
			}
			Ok(())
		}

		fn write_all(&self, mut buf: &[u8]) -> Result<()> {
			while !buf.is_empty() {
				let overlapped = Overlapped::new()?;
				let mut written = 0u32;
				let ok = unsafe { WriteFile(self.handle, Some(buf), Some(&mut written), Some(&overlapped.inner as *const _ as *mut _)) };
				if ok.is_err() {
					let err = unsafe { windows::Win32::Foundation::GetLastError() };
					if err == ERROR_IO_PENDING {
						unsafe { WaitForSingleObject(overlapped.event, INFINITE) };
						let mut actually_written = 0u32;
						let completed = unsafe { GetOverlappedResult(self.handle, &overlapped.inner, &mut actually_written, false) };
						if completed.is_err() || actually_written == 0 {
							bail!("pipe closed or overlapped WriteFile failed");
						}
						written = actually_written;
					} else {
						bail!("pipe closed or WriteFile failed: {err:?}");
					}
				}
				if written == 0 {
					bail!("pipe closed (zero-byte write)");
				}
				buf = &buf[written as usize..];
			}
			Ok(())
		}
	}

	impl Drop for PipeConnection {
		fn drop(&mut self) {
			unsafe {
				CloseHandle(self.handle).ok();
			}
		}
	}
}

#[cfg(unix)]
mod unix_impl {
	use anyhow::{Context as _, Result};
	use std::io::{Read, Write};
	use std::os::unix::net::UnixStream;

	pub struct PipeConnection {
		read_half: std::sync::Mutex<UnixStream>,
		write_half: std::sync::Mutex<UnixStream>,
	}

	impl PipeConnection {
		pub fn connect(socket_path: &str) -> Result<Self> {
			let stream = UnixStream::connect(socket_path).with_context(|| format!("failed to connect to unix socket at {socket_path:?}"))?;
			let read_half = stream.try_clone().context("failed to clone connected socket for independent read/write halves")?;
			Ok(Self { read_half: std::sync::Mutex::new(read_half), write_half: std::sync::Mutex::new(stream) })
		}

		pub fn read_message(&self) -> Result<Vec<u8>> {
			let mut read_half = self.read_half.lock().unwrap();
			let mut len_bytes = [0u8; 4];
			read_half.read_exact(&mut len_bytes).context("pipe closed or read failed (length prefix)")?;
			let len = u32::from_le_bytes(len_bytes) as usize;
			let mut buf = vec![0u8; len];
			read_half.read_exact(&mut buf).context("pipe closed or read failed (payload)")?;
			Ok(buf)
		}

		pub fn write_message(&self, payload: &[u8]) -> Result<()> {
			let len = (payload.len() as u32).to_le_bytes();
			let mut write_half = self.write_half.lock().unwrap();
			write_half.write_all(&len).context("pipe closed or write failed (length prefix)")?;
			write_half.write_all(payload).context("pipe closed or write failed (payload)")?;
			write_half.flush().context("pipe closed or flush failed")?;
			Ok(())
		}
	}
}

#[cfg(windows)]
pub use windows_impl::PipeConnection;
#[cfg(unix)]
pub use unix_impl::PipeConnection;
