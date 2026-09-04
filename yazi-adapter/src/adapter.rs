use std::{fmt::{self, Debug}, path::PathBuf, sync::OnceLock};

use anyhow::{Result, bail};
use ratatui_core::layout::Rect;
use yazi_emulator::EMULATOR;
use yazi_shim::cell::SyncCell;
use yazi_widgets::clear::ClearInventory;

use crate::{ADAPTOR, drivers::{Driver, Drivers, Srp}};

#[derive(Default)]
pub struct Adapter {
	driver:        OnceLock<Driver>,
	shown:         SyncCell<Option<Rect>>,
	pub collision: SyncCell<bool>,
	/// The `(session_id, file_id)` of the most recently shown audio/video
	/// placement, if any — set by `audio_show`/`Srp::stream_video` (via
	/// `image_show`'s routing), read and cleared by the NEXT `*_show` call
	/// of any kind so it can tell Som to stop that OLD placement's player
	/// before printing a new placeholder grid. See `SrvRequest::
	/// StopPlayback`'s own doc comment (in Som's repo) for the full
	/// rationale — yazi's preview pane has no `clear`-equivalent signal
	/// when the cursor moves to a different file, so this is the only way
	/// Som learns the old placement was abandoned.
	last_media_id: SyncCell<Option<(u32, u32)>>,
}

impl Debug for Adapter {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.driver.get() {
			Some(driver) => driver.fmt(f),
			None => f.write_str("Pending"),
		}
	}
}

impl Adapter {
	pub async fn image_show<P>(&self, path: P, max: Rect) -> Result<Rect>
	where
		P: Into<PathBuf>,
	{
		self.stop_previous_media();

		let probe = &EMULATOR.probe;
		probe.wait(probe.id.get()).await;

		let driver = self.driver.get_or_init(|| {
			let driver = Drivers::matches(&EMULATOR);
			driver.start();
			driver
		});
		driver.image_show(path, max).await
	}

	/// Fires `SrvChannel::stop_playback` for whatever `(session_id,
	/// file_id)` `record_media_id` last recorded, then clears it — called
	/// at the START of every `*_show` method (image/audio/markdown all
	/// share this ONE call site, since any of them can follow an
	/// audio/video placement in yazi's own preview cursor movement), so a
	/// stale player never survives past the very next preview. A no-op if
	/// nothing was recorded (the common case: images/markdown never
	/// record anything, and this is the very first preview shown at all).
	///
	/// Fired on a background OS thread, NOT awaited or run inline —
	/// `Srp::stop_playback` is a blocking, synchronous connect-and-send
	/// (`SrvChannel::stop_playback`'s own `daemon::connect_or_spawn`),
	/// and this method is called from inside every `*_show`'s async body
	/// on yazi's own async executor. Running it inline there blocked
	/// EVERY subsequent preview (image, audio, video, markdown alike) on
	/// this connect completing first — confirmed live as a real
	/// regression: nothing previewed at all once this call was added
	/// inline. Mirrors the exact same fire-and-forget shape Som's own
	/// `rich_content_srv_channel::request_byte_range` already uses for
	/// the identical reason (see that function's own doc comment).
	/// Best-effort like every other `som-srv` message in this driver — a
	/// failed send (daemon not running, connection error) is logged and
	/// swallowed, not surfaced to the caller.
	fn stop_previous_media(&self) {
		let Some((session_id, file_id)) = self.last_media_id.replace(None) else { return };
		std::thread::spawn(move || {
			if let Err(err) = Srp::stop_playback(session_id, file_id) {
				yazi_macro::error!("[Adapter] failed to send StopPlayback for {session_id:#x}:{file_id:#x}: {err:#}");
			}
		});
	}

	/// Called by `Srp::audio_show`/`stream_video` right after they've
	/// picked their `(session_id, file_id)` — see `stop_previous_media`'s
	/// own doc comment for the other half of this mechanism.
	pub(super) fn record_media_id(&self, session_id: u32, file_id: u32) {
		self.last_media_id.set(Some((session_id, file_id)));
	}

	pub fn image_hide(&self) -> Result<()> {
		let Some(area) = self.shown.replace(None) else { return Ok(()) };
		match self.driver.get() {
			Some(driver) => driver.image_erase(area),
			None => Ok(()),
		}
	}

	/// Audio's counterpart to [`Self::image_show`] — unlike every other
	/// content type this adapter shows, only [`Driver::Srp`] speaks audio
	/// at all (decoding/playback happen on Som's own receiving end, not
	/// in this process at all — see `Srp::audio_show`'s own doc comment),
	/// so this bypasses `Driver::image_show`'s per-driver dispatch and
	/// calls `Srp` directly. Errors instead of silently doing nothing if
	/// the resolved driver isn't `Srp` — a caller (the `audio.lua`
	/// previewer) is expected to check `SOM_WINDOW_ID`/`Brand::Som`
	/// itself before ever reaching this call, so landing here on a
	/// different driver means that check was skipped or wrong, not a
	/// normal "this terminal doesn't support audio previews" case that
	/// should fail quietly.
	pub async fn audio_show<P>(&self, path: P, max: Rect) -> Result<Rect>
	where
		P: Into<PathBuf>,
	{
		self.stop_previous_media();

		let probe = &EMULATOR.probe;
		probe.wait(probe.id.get()).await;

		let driver = self.driver.get_or_init(|| {
			let driver = Drivers::matches(&EMULATOR);
			driver.start();
			driver
		});
		if *driver != Driver::Srp {
			bail!("audio preview requires Som's SRP driver, resolved driver is {driver}");
		}

		Srp::audio_show(path.into(), max).await
	}

	/// Markdown's counterpart to [`Self::audio_show`] — same "SRP-only,
	/// bypass per-driver dispatch, call `Srp` directly" shape and the same
	/// reasoning: markdown parsing/layout/rendering all happen on Som's
	/// own receiving end (see `Srp::markdown_show`'s own doc comment), not
	/// in this process, so no other `Driver` variant has anything
	/// meaningful to do with a `.md` file either.
	pub async fn markdown_show<P>(&self, path: P, max: Rect) -> Result<Rect>
	where
		P: Into<PathBuf>,
	{
		self.stop_previous_media();

		let probe = &EMULATOR.probe;
		probe.wait(probe.id.get()).await;

		let driver = self.driver.get_or_init(|| {
			let driver = Drivers::matches(&EMULATOR);
			driver.start();
			driver
		});
		if *driver != Driver::Srp {
			bail!("markdown preview requires Som's SRP driver, resolved driver is {driver}");
		}

		Srp::markdown_show(path.into(), max).await
	}

	pub fn shown_area(&self) -> Option<Rect> { self.shown.get() }

	pub(super) fn shown_store(&self, area: Rect) { self.shown.set(Some(area)); }
}

inventory::submit! {
	ClearInventory {
		clear: |area| {
			let overlap = area.intersection(ADAPTOR.shown.get()?);
			if overlap.area() == 0 {
				return None;
			}

			ADAPTOR.driver.get()?.image_erase(overlap).ok();
			ADAPTOR.collision.set(true);
			Some(overlap)
		},
	}
}
