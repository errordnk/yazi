use core::str;
use std::{io::Write, path::PathBuf};

use anyhow::Result;
use base64::{Engine, engine::general_purpose};
use image::DynamicImage;
use ratatui_core::{layout::Rect, style::Color};
use yazi_config::THEME;
use yazi_emulator::{CLOSE, ESCAPE, Emulator, START};
use yazi_shim::cell::SyncCell;
use yazi_tty::sequence::{MoveTo, ResetAttrs, SetBg, SetFg};

use crate::{ADAPTOR, drivers::diacritics::DIACRITICS, image::Image};

pub(super) struct Kgp;

impl Kgp {
	pub(super) async fn image_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let img = Image::downscale(path, max).await?;
		let area = Image::pixel_area((img.width(), img.height()), max);

		let b1 = Self::encode(img).await?;
		let b2 = Self::place(&area)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&b1)?;
			w.write_all(&b2)?;
			Ok(area)
		})
	}

	pub(super) fn image_erase(area: Rect) -> Result<()> {
		let s = " ".repeat(area.width as usize);
		Emulator::move_lock((0, 0), |w| {
			if let Some(c) = THEME.app.overall.get().bg {
				write!(w, "{}", SetBg(c))?;
			}

			for y in area.top()..area.bottom() {
				write!(w, "{}", MoveTo(area.x, y))?;
				write!(w, "{s}")?;
			}

			write!(w, "{ResetAttrs}{START}_Gq=2,a=d,d=A{ESCAPE}\\{CLOSE}")?;
			Ok(())
		})
	}

	async fn encode(img: DynamicImage) -> Result<Vec<u8>> {
		fn output(raw: &[u8], format: u8, size: (u32, u32)) -> Result<Vec<u8>> {
			let b64 = general_purpose::STANDARD.encode(raw).into_bytes();

			let mut it = b64.chunks(4096).peekable();
			let mut buf = Vec::with_capacity(b64.len() + it.len() * 50);
			if let Some(first) = it.next() {
				write!(
					buf,
					"{START}_Gq=2,a=T,C=1,U=1,f={format},s={},v={},i={},m={};{}{ESCAPE}\\{CLOSE}",
					size.0,
					size.1,
					Kgp::image_id(),
					it.peek().is_some() as u8,
					unsafe { str::from_utf8_unchecked(first) },
				)?;
			}

			while let Some(chunk) = it.next() {
				write!(buf, "{START}_Gm={};{}{ESCAPE}\\{CLOSE}", it.peek().is_some() as u8, unsafe {
					str::from_utf8_unchecked(chunk)
				})?;
			}

			write!(buf, "{CLOSE}")?;
			Ok(buf)
		}

		let size = (img.width(), img.height());
		tokio::task::spawn_blocking(move || match img {
			DynamicImage::ImageRgb8(v) => output(v.as_raw(), 24, size),
			DynamicImage::ImageRgba8(v) => output(v.as_raw(), 32, size),
			v => output(v.into_rgb8().as_raw(), 24, size),
		})
		.await?
	}

	fn place(area: &Rect) -> Result<Vec<u8>> {
		let mut buf = Vec::with_capacity(area.width as usize * area.height as usize * 3 + 500);

		let id = Self::image_id();
		let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
		write!(buf, "{}", SetFg(Color::Rgb(r as u8, g as u8, b as u8)))?;

		if let Some(c) = THEME.app.overall.get().bg {
			write!(buf, "{}", SetBg(c))?;
		}

		for y in 0..area.height {
			write!(buf, "{}", MoveTo(area.x, area.y + y))?;
			for x in 0..area.width {
				write!(buf, "\u{10EEEE}")?;
				write!(buf, "{}", *DIACRITICS.get(y as usize).unwrap_or(&DIACRITICS[0]))?;
				write!(buf, "{}", *DIACRITICS.get(x as usize).unwrap_or(&DIACRITICS[0]))?;
			}
		}

		write!(buf, "{ResetAttrs}")?;
		Ok(buf)
	}

	fn image_id() -> u32 {
		static CACHE: SyncCell<Option<u32>> = SyncCell::new(None);
		match CACHE.get() {
			Some(n) => n,
			None => {
				let n = std::process::id() % (0xffffff + 1);
				CACHE.set(Some(n));
				n
			}
		}
	}
}
