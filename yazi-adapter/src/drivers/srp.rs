//! Driver for Som's own Rich Protocol (SRP) — a from-scratch terminal
//! graphics protocol, not the Kitty graphics protocol this file sits next
//! to. See `SRP_INTEGRATION_GUIDE.md` in the Som repository
//! (<https://github.com/errordnk/som>) for the full wire-format spec this
//! module implements; this file exists as that guide's worked example of
//! a third-party TUI adding SRP support.
//!
//! Two things distinguish SRP from Kitty's protocol enough to need a
//! dedicated driver instead of reusing [`super::Kgp`]:
//!
//! 1. **Wire encoding**: SRP base91-encodes the ENTIRE envelope (header
//!    and payload), not just the payload, and uses its own envelope
//!    layout (`ESC _ S <header_b91> <SEPARATOR> <payload_b91> ESC \`,
//!    marker byte `S` not `G`) — seeSRP_INTEGRATION_GUIDE.md's "Wire
//!    format" section for why (Windows ConPTY's console-codepage
//!    reinterpretation of raw bytes >= 0x80, which base91 sidesteps the
//!    same way Kitty's own base64 payload does, just with less overhead).
//! 2. **Placement rendering**: SRP always uses the Unicode-placeholder
//!    grid technique (`U=1` in Kitty's own terms) — there is no
//!    pixel-anchored placement mode at all, so this driver doesn't need
//!    Kgp's `a=T`/`a=p` command distinction.
//!
//! The diacritic table below is IDENTICAL to [`super::kgp::DIACRITICS`]
//! (both implement the same third-party spec, Kitty's own "Unicode
//! placeholders" extension) — kept as a separate copy rather than a
//! shared `pub(super)` item because the two drivers are meant to stay
//! independently portable reference implementations for two unrelated
//! protocols that just happen to reuse the same third piece of prior art.

use std::{io::Write, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

use anyhow::{Result, bail};
use image::{ImageDecoder, ImageReader};
use ratatui_core::layout::Rect;
use yazi_emulator::Emulator;
use yazi_tty::TTY;

use crate::{ADAPTOR, drivers::diacritics::DIACRITICS, image::Image};

/// SRP envelope marker byte (`ESC _ S ...`), distinguishing this
/// protocol's APC strings from Kitty's own (`ESC _ G ...`) on the same
/// wire. See SRP_INTEGRATION_GUIDE.md's "Envelope format" section.
const MARKER: u8 = b'S';

/// Wire format version this driver speaks — must match Som's own
/// `rich_content_transport::VERSION`. A receiver on an older/newer Som
/// build would reject an envelope with a mismatched version rather than
/// guess at a different layout.
const VERSION: u8 = 1;

/// `ContentType` byte for a still or animated raster image, one of six
/// values `rich_content_transport::ContentType` defines. This driver
/// sends GIF/JPEG/PNG (`ContentMetadata::Image`) and, for audio, MP3/FLAC
/// (`ContentMetadata::Audio`) — Markdown/Video never appear here.
const CONTENT_TYPE_GIF: u8 = 0;
const CONTENT_TYPE_AUDIO: u8 = 1;
const CONTENT_TYPE_JPEG: u8 = 4;
const CONTENT_TYPE_PNG: u8 = 5;

/// `ContentMetadata` discriminant tags — see SRP_INTEGRATION_GUIDE.md's
/// "ContentMetadata" section for the full four-variant layout and why
/// every variant pads out to the same encoded length regardless of which
/// one a chunk actually carries.
const METADATA_TAG_IMAGE: u8 = 0;
const METADATA_TAG_AUDIO: u8 = 1;

/// How many raw bytes the encoded `ContentMetadata` occupies after its
/// 1-byte discriminant — fixed at the longest variant's size (Video:
/// 4+4+4+4+1 = 17 bytes) so every variant, including the zero-field
/// Markdown one, has the same on-wire length. `image_show` writes the
/// `Image` variant's 10 used bytes; `audio_show` writes the `Audio`
/// variant's 9 used bytes (`sample_rate: u32` + `channels: u8` +
/// `bits_per_sample: u8` + `duration_ms: u32`) — both zero-pad the rest.
const METADATA_FIELDS_LEN: usize = 17;

/// Fixed cell footprint for the audio widget Som paints — audio has no
/// pixel dimensions to derive a footprint from the way an image does, so
/// this driver reserves a constant-size placeholder grid instead, same
/// principle `somcat`'s own `AUDIO_WIDGET_COLUMNS`/`AUDIO_WIDGET_ROWS`
/// constants use (kept in sync with that reference client's values: 40
/// columns for the play/pause glyph, seek bar, and elapsed/total time
/// text, plus 2 more for a trailing close glyph).
const AUDIO_WIDGET_COLUMNS: u16 = 42;
const AUDIO_WIDGET_ROWS: u16 = 1;

/// Every printable ASCII byte (`0x21..=0x7E`) except `"`, `'`, `\` — 91
/// symbols. See SRP_INTEGRATION_GUIDE.md's "Why base91" section for the
/// full rationale (confirmed byte-for-byte lossless across a real Windows
/// ConPTY round-trip, where anything >= 0x80 gets silently mangled by the
/// active console codepage).
const BASE91_ALPHABET: [u8; 91] = build_base91_alphabet();

const fn build_base91_alphabet() -> [u8; 91] {
	let mut alphabet = [0u8; 91];
	let mut i = 0;
	let mut b = 0x21u8;
	while b <= 0x7E {
		if b != b'"' && b != 0x27 && b != 0x5C {
			alphabet[i] = b;
			i += 1;
		}
		b += 1;
	}
	alphabet
}

/// Encodes `data` with [`BASE91_ALPHABET`] using the standard basE91
/// bit-packing algorithm — see SRP_INTEGRATION_GUIDE.md's "Why base91"
/// section for the bit-packing rationale (13 vs 14 bits consumed per
/// output symbol pair, decided by the data itself).
fn base91_encode(data: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(data.len() * 2 + 2);
	let mut bit_buf: u32 = 0;
	let mut bit_count: u32 = 0;
	for &byte in data {
		bit_buf |= (byte as u32) << bit_count;
		bit_count += 8;
		if bit_count > 13 {
			let mut v = bit_buf & 8191; // low 13 bits
			if v > 88 {
				bit_buf >>= 13;
				bit_count -= 13;
			} else {
				v = bit_buf & 16383; // low 14 bits
				bit_buf >>= 14;
				bit_count -= 14;
			}
			out.push(BASE91_ALPHABET[(v % 91) as usize]);
			out.push(BASE91_ALPHABET[(v / 91) as usize]);
		}
	}
	if bit_count > 0 {
		out.push(BASE91_ALPHABET[(bit_buf % 91) as usize]);
		if bit_count > 7 || bit_buf > 90 {
			out.push(BASE91_ALPHABET[(bit_buf / 91) as usize]);
		}
	}
	out
}

/// CRC32 (IEEE 802.3 polynomial) over the raw (pre-base91) payload —
/// same table-free bit-by-bit implementation Som's own sender uses, kept
/// local rather than pulling in a crate since this is the only place in
/// this driver that needs a checksum.
fn crc32(data: &[u8]) -> u32 {
	const POLY: u32 = 0xEDB8_8320;
	let mut crc = 0xFFFF_FFFFu32;
	for &byte in data {
		crc ^= byte as u32;
		for _ in 0..8 {
			let mask = (crc & 1).wrapping_neg();
			crc = (crc >> 1) ^ (POLY & mask);
		}
	}
	!crc
}

/// Builds one complete SRP envelope (marker + base91-encoded header +
/// separator + base91-encoded payload), ready to be wrapped in
/// `ESC _ ... ESC \` and written to the PTY. Mirrors Som's own
/// `rich_content_transport::build_envelope` field-for-field — see
/// SRP_INTEGRATION_GUIDE.md's "Envelope format" section for the exact
/// byte layout this must match. `metadata_tag`/`metadata_fields` are the
/// caller's already-encoded `ContentMetadata` variant (see
/// [`METADATA_TAG_IMAGE`]/[`METADATA_TAG_AUDIO`]) — this function itself
/// stays content-type-agnostic, same as Som's own receiving-side parser.
#[allow(clippy::too_many_arguments)]
fn build_envelope(
	content_type: u8,
	session_id: u32,
	file_id: u32,
	chunk_offset: u64,
	total_size: u64,
	metadata_tag: u8,
	metadata_fields: [u8; METADATA_FIELDS_LEN],
	payload: &[u8],
) -> Vec<u8> {
	let mut fields = Vec::with_capacity(1 + 1 + 4 + 4 + 8 + 4 + 8 + 1 + METADATA_FIELDS_LEN + 4);
	fields.push(VERSION);
	fields.push(content_type);
	fields.extend_from_slice(&session_id.to_le_bytes());
	fields.extend_from_slice(&file_id.to_le_bytes());
	fields.extend_from_slice(&chunk_offset.to_le_bytes());
	fields.extend_from_slice(&(payload.len() as u32).to_le_bytes());
	fields.extend_from_slice(&total_size.to_le_bytes());

	// Zero-padded to METADATA_FIELDS_LEN like every ContentMetadata variant
	// — see this module's METADATA_FIELDS_LEN doc comment.
	fields.push(metadata_tag);
	fields.extend_from_slice(&metadata_fields);

	fields.extend_from_slice(&crc32(payload).to_le_bytes());

	let header_b91 = base91_encode(&fields);

	let mut envelope = Vec::with_capacity(1 + header_b91.len() + 1 + payload.len() * 2);
	envelope.push(MARKER);
	envelope.extend_from_slice(&header_b91);
	envelope.push(b' '); // SEPARATOR — see SRP_INTEGRATION_GUIDE.md for why this can't be a fixed byte offset instead.
	envelope.extend_from_slice(&base91_encode(payload));
	envelope
}

pub struct Srp;

impl Srp {
	pub(super) async fn image_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let bytes = tokio::fs::read(&path).await?;
		// Guessed from the file's own magic bytes, not its extension — `path`
		// here is frequently a hashed cache file yazi itself created
		// (`ya.file_cache`'s `Twox128`-derived filename, see this driver's
		// doc comment), which has no extension to check at all. `image`'s
		// own format sniffing already reads the handful of leading bytes
		// every one of these formats identifies itself with.
		let format = image::guess_format(&bytes)?;
		let content_type = match format {
			image::ImageFormat::Gif => CONTENT_TYPE_GIF,
			image::ImageFormat::Jpeg => CONTENT_TYPE_JPEG,
			image::ImageFormat::Png => CONTENT_TYPE_PNG,
			other => bail!("SRP driver only supports Gif/Jpeg/Png, got {other:?}"),
		};

		let (width_px, height_px, is_animated) =
			tokio::task::spawn_blocking({
				let path = path.clone();
				move || Self::probe_dimensions(&path, content_type)
			})
			.await??;

		// session_id/file_id both time-derived and masked to 24 bits — see
		// SRP_INTEGRATION_GUIDE.md's "Session and file ids" section for why
		// (they're painted into a placeholder cell's 24-bit RGB color, so an
		// unmasked id would silently lose its high byte on the round trip).
		let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
		let session_id = (now_ms & 0xFF_FFFF).max(1);
		let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1);

		// The placeholder grid is printed FIRST, before a single byte of the
		// file has gone out — its footprint only depends on `width_px`/
		// `height_px` (already known from the format header probed above),
		// not on how much of the file has arrived. This is what makes
		// display progressive on Som's receiving end at all: Som decodes and
		// paints whatever prefix of the file has landed in its cache as soon
		// as that prefix is a valid partial GIF (see
		// `rich_content_gif_player`'s incremental decoder in the Som
		// repository), completely independent of when the placeholder text
		// itself appeared — so `image_show` returning right after this print
		// call, instead of after the whole transfer, is what lets the first
		// frame land as early as Som is able to produce it rather than only
		// once every chunk has been sent.
		let area = Image::pixel_area((width_px, height_px), max);
		let b2 = Self::place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&b2)?;
			Ok(area)
		})?;

		// Streaming itself is fire-and-forget on a plain `tokio::spawn`, NOT
		// awaited here and NOT `spawn_blocking` — two separate reasons:
		//
		// 1. `image_show`'s caller (yazi's own preview machinery) is what
		//    the rest of this driver's `image_show`/`image_erase` contract
		//    is measured against: it wants an area back promptly to lay out
		//    the rest of the UI, not "wait until the last byte of a GIF that
		//    might be tens of megabytes has left this process." The
		//    placeholder text above already reserves that area and is
		//    already valid, real content from the terminal's point of
		//    view — nothing downstream needs the transfer to be done first.
		// 2. Base91-encoding every chunk plus a CRC32 over its payload is
		//    real CPU work (hundreds of iterations for a large GIF) — moved
		//    to `spawn_blocking` internally (see below) so it never runs on
		//    an async worker thread that yazi's own UI redraw loop also
		//    depends on, the same reasoning as before, just no longer
		//    blocking `image_show`'s own return on top of it.
		//
		// A transfer failure here has nowhere to go but a dropped `Result`
		// (this task's return value is never awaited) — acceptable because
		// the alternative (propagating it back through `image_show`, which
		// has already returned) isn't available, and a failed transfer is
		// already visible to the user as "the placeholder never fills in,"
		// the same failure mode Som's own progressive design has to tolerate
		// from a network/PTY hiccup regardless of which client is sending.
		tokio::spawn(async move {
			let result = tokio::task::spawn_blocking(move || -> Result<()> {
				const CHUNK_SIZE: usize = 3800;
				let mut offset = 0u64;
				// Written directly through the TTY lock, NOT
				// `Emulator::move_lock` — that helper issues a cursor-
				// position save/move/restore dance around its callback
				// (needed for visible content like Kgp's pixel-anchored
				// placements, which must land at a specific screen cell),
				// but these APC envelopes carry no visible output at all:
				// nothing in an APC string paints anywhere on screen, so
				// there is nothing for a cursor move to protect. Wrapping
				// 47+ chunk writes (typical for a multi-frame GIF) in
				// `move_lock` each round-trips the cursor to (0,0) and back
				// on every single chunk — pure overhead, and on Windows
				// specifically (`move_lock`'s own `cfg!(windows)` branch)
				// each round-trip also sleeps 1ms and re-touches cursor
				// visibility three times, which measurably slowed multi-
				// chunk transfers without helping — the actual position
				// that matters was already set once, correctly, by the
				// `place` call above, before this task was even spawned.
				let mut metadata_fields = [0u8; METADATA_FIELDS_LEN];
				metadata_fields[0..4].copy_from_slice(&width_px.to_le_bytes());
				metadata_fields[4..8].copy_from_slice(&height_px.to_le_bytes());
				metadata_fields[8] = 32; // color_bits: every pixel this driver sends is RGBA on Som's receiving end.
				metadata_fields[9] = is_animated as u8;

				let mut w = TTY.lockout();
				for payload in bytes.chunks(CHUNK_SIZE) {
					let envelope = build_envelope(
						content_type,
						session_id,
						file_id,
						offset,
						bytes.len() as u64,
						METADATA_TAG_IMAGE,
						metadata_fields,
						payload,
					);
					w.write_all(&[0x1B, b'_'])?;
					w.write_all(&envelope)?;
					w.write_all(&[0x1B, b'\\'])?;
					offset += payload.len() as u64;
				}
				w.flush()?;
				Ok(())
			})
			.await;
			if let Err(e) = result.unwrap_or_else(|e| Err(e.into())) {
				yazi_macro::error!("[Srp] streaming {path:?} failed: {e}");
			}
		});

		Ok(area)
	}

	pub(super) fn image_erase(area: Rect) -> Result<()> {
		let s = " ".repeat(area.width as usize);
		Emulator::move_lock((0, 0), |w| {
			for y in area.top()..area.bottom() {
				write!(w, "\x1b[{};{}H", y + 1, area.x + 1)?;
				write!(w, "{s}")?;
			}
			Ok(())
		})
	}

	/// Audio's counterpart to [`Self::image_show`]. `somcat`'s own
	/// `stream_file` — see SRP_INTEGRATION_GUIDE.md's "Audio: streamed
	/// progressively" section — decoding/playback both happen on Som's
	/// receiving end, not here; this driver's only job is to probe the
	/// file's header metadata, reserve the widget's fixed footprint, and
	/// stream the raw bytes.
	///
	/// No byte-range query responder — unlike `somcat`, which keeps
	/// running (and the file handle open) for as long as its own process
	/// lives specifically to answer Som's seek-past-cached-prefix
	/// queries, this driver's whole request is already in memory
	/// (`bytes`, read once up front) and the surrounding yazi process
	/// keeps running regardless of this particular file's preview — but
	/// yazi has no long-lived per-file background reader watching its own
	/// stdin for `QUERY_MARKER` envelopes the way `somcat` does, so a
	/// user seeking past whatever prefix has already streamed on a large,
	/// still-in-progress transfer won't get an answer here. Acceptable
	/// for a file manager's preview pane (a user browsing files is
	/// unlikely to be mid-seek on a multi-gigabyte FLAC preview the same
	/// way they might be with a dedicated player), unlike `somcat`'s own
	/// no-query-support gap which the reference client at least documents
	/// as a real limitation worth fixing for.
	pub async fn audio_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let bytes = tokio::fs::read(&path).await?;

		let (sample_rate, channels, bits_per_sample, duration_ms) =
			tokio::task::spawn_blocking({
				let path = path.clone();
				move || Self::probe_audio_metadata(&path)
			})
			.await??;

		let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
		let session_id = (now_ms & 0xFF_FFFF).max(1);
		let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1);

		// Fixed footprint, clamped to whatever preview area is actually
		// available — audio has no pixel dimensions to derive a footprint
		// from the way an image does (see AUDIO_WIDGET_COLUMNS/ROWS's own
		// doc comment).
		let area = Rect {
			x:      max.x,
			y:      max.y,
			width:  AUDIO_WIDGET_COLUMNS.min(max.width),
			height: AUDIO_WIDGET_ROWS.min(max.height),
		};
		let b2 = Self::place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&b2)?;
			Ok(area)
		})?;

		// Same "don't block the caller on the transfer" reasoning as
		// `image_show` above — see that method's own doc comment for the
		// full rationale (fire-and-forget `tokio::spawn` wrapping a
		// `spawn_blocking` chunk loop).
		tokio::spawn(async move {
			let result = tokio::task::spawn_blocking(move || -> Result<()> {
				const CHUNK_SIZE: usize = 3800;
				let mut offset = 0u64;

				let mut w = TTY.lockout();
				for payload in bytes.chunks(CHUNK_SIZE) {
					let mut metadata_fields = [0u8; METADATA_FIELDS_LEN];
					// ContentMetadata::Audio field order: sample_rate(4) +
					// channels(1) + bits_per_sample(1) + duration_ms(4) — see
					// SRP_INTEGRATION_GUIDE.md's "ContentMetadata" table.
					metadata_fields[0..4].copy_from_slice(&sample_rate.to_le_bytes());
					metadata_fields[4] = channels;
					metadata_fields[5] = bits_per_sample;
					metadata_fields[6..10].copy_from_slice(&duration_ms.to_le_bytes());

					let envelope = build_envelope(
						CONTENT_TYPE_AUDIO,
						session_id,
						file_id,
						offset,
						bytes.len() as u64,
						METADATA_TAG_AUDIO,
						metadata_fields,
						payload,
					);
					w.write_all(&[0x1B, b'_'])?;
					w.write_all(&envelope)?;
					w.write_all(&[0x1B, b'\\'])?;
					offset += payload.len() as u64;
				}
				w.flush()?;
				Ok(())
			})
			.await;
			if let Err(e) = result.unwrap_or_else(|e| Err(e.into())) {
				yazi_macro::error!("[Srp] streaming {path:?} failed: {e}");
			}
		});

		Ok(area)
	}

	/// Probes `sample_rate`/`channels`/`bits_per_sample`/`duration_ms`
	/// from the file's own container header via `symphonia`'s format
	/// reader — no full decode, mirroring `somcat`'s own
	/// `audio_metadata()`. `duration_ms` comes from
	/// `CodecParameters::n_frames`/`time_base` (an MP3's Xing/VBRI VBR
	/// tag or size estimate, a FLAC's STREAMINFO block) — `0` if the
	/// format reader couldn't determine it, same "unknown, don't guess"
	/// convention every other SRP metadata field uses.
	fn probe_audio_metadata(path: &std::path::Path) -> Result<(u32, u8, u8, u32)> {
		use symphonia::core::codecs::CODEC_TYPE_NULL;
		use symphonia::core::io::MediaSourceStream;
		use symphonia::core::probe::Hint;

		let file = std::fs::File::open(path)?;
		let mss = MediaSourceStream::new(Box::new(file), Default::default());

		let mut hint = Hint::new();
		if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
			hint.with_extension(extension);
		}

		let probed = symphonia::default::get_probe().format(&hint, mss, &Default::default(), &Default::default())?;

		let track = probed
			.format
			.tracks()
			.iter()
			.find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
			.ok_or_else(|| anyhow::anyhow!("{path:?}: no decodable audio track found"))?;

		let params = &track.codec_params;
		let sample_rate = params.sample_rate.ok_or_else(|| anyhow::anyhow!("{path:?}: unknown sample rate"))?;
		let channels = params
			.channels
			.map(|c| c.count() as u8)
			.ok_or_else(|| anyhow::anyhow!("{path:?}: unknown channel count"))?;
		let bits_per_sample = params.bits_per_sample.map(|b| b as u8).unwrap_or(16);
		let duration_ms = match (params.n_frames, params.time_base) {
			(Some(n_frames), Some(time_base)) => {
				let time = time_base.calc_time(n_frames);
				(time.seconds.saturating_mul(1000) as u32).saturating_add((time.frac * 1000.0) as u32)
			},
			_ => 0,
		};

		Ok((sample_rate, channels, bits_per_sample, duration_ms))
	}

	/// Reads just the format header — width/height for every supported
	/// format, plus a second-frame byte-scan for GIF animation detection —
	/// without decoding any pixel data. Mirrors `somcat`'s own
	/// `gif_metadata`/`static_image_metadata` (see SRP_INTEGRATION_GUIDE.md's
	/// "Client-side metadata extraction" section for why this scan is a
	/// safe heuristic rather than a full GIF parse).
	fn probe_dimensions(path: &std::path::Path, content_type: u8) -> Result<(u32, u32, bool)> {
		if content_type == CONTENT_TYPE_GIF {
			let bytes = std::fs::read(path)?;
			let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&bytes))?;
			let (width, height) = decoder.dimensions();
			// A second Image Descriptor block (0x2C) before EOF means more than
			// one frame — cheap animation check without decoding any frame.
			let is_animated = bytes.windows(1).skip(13).filter(|w| w[0] == 0x2C).count() > 1;
			Ok((width, height, is_animated))
		} else {
			let reader = ImageReader::open(path)?.with_guessed_format()?;
			let (width, height) = reader.into_dimensions()?;
			Ok((width, height, false))
		}
	}

	fn place(area: &Rect, session_id: u32, file_id: u32) -> Result<Vec<u8>> {
		let mut buf = Vec::with_capacity(area.width as usize * area.height as usize * 3 + 500);

		let (sr, sg, sb) = id_to_rgb(session_id);
		let (fr, fg, fb) = id_to_rgb(file_id);
		write!(buf, "\x1b[38;2;{sr};{sg};{sb}m\x1b[58;2;{fr};{fg};{fb}m")?;

		for y in 0..area.height {
			write!(buf, "\x1b[{};{}H", area.y + y + 1, area.x + 1)?;
			for x in 0..area.width {
				write!(buf, "\u{10EEEE}")?;
				write!(buf, "{}", *DIACRITICS.get(y as usize).unwrap_or(&DIACRITICS[0]))?;
				write!(buf, "{}", *DIACRITICS.get(x as usize).unwrap_or(&DIACRITICS[0]))?;
			}
		}

		write!(buf, "\x1b[0m")?;
		Ok(buf)
	}
}

/// Foreground carries session_id, underline color carries file_id — same
/// split Som's own `somcat` client uses, see that tool's `id_to_rgb`.
fn id_to_rgb(id: u32) -> (u8, u8, u8) { (((id >> 16) & 0xFF) as u8, ((id >> 8) & 0xFF) as u8, (id & 0xFF) as u8) }
