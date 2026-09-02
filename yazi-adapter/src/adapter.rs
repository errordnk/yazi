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
		let probe = &EMULATOR.probe;
		probe.wait(probe.id.get()).await;

		let driver = self.driver.get_or_init(|| {
			let driver = Drivers::matches(&EMULATOR);
			driver.start();
			driver
		});
		driver.image_show(path, max).await
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
