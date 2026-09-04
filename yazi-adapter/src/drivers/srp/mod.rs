//! Driver for Som's own Rich Protocol (SRP) — a from-scratch terminal
//! graphics protocol, not the Kitty graphics protocol this file sits next
//! to. See `SRP_INTEGRATION_GUIDE.md` in the Som repository
//! (<https://github.com/errordnk/som>) for the full wire-format spec this
//! module implements.
//!
//! This is a from-scratch rewrite of the original base91/APC-over-PTY
//! implementation, targeting Som's CURRENT transport: bulk payload and
//! byte-range query/response travel over
//! `som-srv`'s binary side channel (`srv_channel::SrvChannel`, ported
//! from `crates/somcat/src/srv_channel.rs` — see `protocol.rs`'s own doc
//! comment for why this is a hand-kept port rather than a shared
//! dependency on the `som_srv` crate), mirroring Som's own `somcat`
//! reference client field-for-field and behavior-for-behavior — the
//! explicit goal being that whatever works through `somcat` works
//! through this driver, the same way. The placeholder-grid control
//! handshake (Unicode private-use codepoint + diacritics, encoding
//! `(session_id, file_id)` into a cell's foreground/underline color)
//! stays on the ordinary PTY, unaffected by any of this — it never
//! depended on the old transport either.
//!
//! Three content families, one driver:
//!
//! - **Image/GIF** (`image_show`'s only job before this rewrite): reads
//!   the whole file into memory (small enough that streaming-from-disk
//!   was never worth the complexity — mirrors `somcat::stream_file`'s
//!   own reasoning for keeping this branch simple), probes width/height/
//!   animation from the format header, then sends the whole buffer as
//!   `PutChunk`s.
//! - **Video**: streamed off disk in bounded chunks (never the whole
//!   file into memory — real movie files can be double-digit gigabytes),
//!   with a dedicated byte-range-responder connection so a seek on Som's
//!   side gets an answer promptly instead of queueing behind a large
//!   in-flight sequential transfer (mirrors `somcat::stream_file_from_
//!   disk`/`spawn_byte_range_responder_from_disk`/`send_range_chunks_
//!   from_disk_interruptible` exactly — see those functions' own doc
//!   comments in Som for the full history of bugs this design fixes).
//! - **Audio**: same disk-streaming path as video, minus any format
//!   probing (this driver has no symphonia/FFmpeg dependency — see
//!   `metadata.rs`'s own doc comment for why real metadata probing is
//!   deliberately NOT ported here).

mod daemon;
mod markdown_line_count;
mod metadata;
mod pipe;
mod protocol;
mod srv_channel;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use ratatui_core::layout::Rect;
use yazi_emulator::{Dimension, Emulator};

use crate::{ADAPTOR, drivers::diacritics::DIACRITICS, image::Image};
use protocol::{ContentMetadata, ContentType};
use srv_channel::SrvChannel;

/// One piece of a file transfer's on-wire chunk — same size `somcat`
/// uses, see that constant's own doc comment for why this number in
/// particular (no longer PTY-codepage-driven now that payload travels
/// over a raw binary side channel, but no reason to pick a different
/// number either).
const CHUNK_SIZE: usize = 65536;

/// Fixed placeholder footprint for video/audio when this driver has no
/// real dimensions to reserve — see `metadata.rs`'s own doc comment for
/// why real ffprobe-style dimensions aren't available here. Mirrors
/// `somcat::VIDEO_PLACEHOLDER_WIDTH_PX`/`HEIGHT_PX` exactly; Som's own
/// paint path never shrinks a placement's footprint once printed, but
/// happily accepts an inaccurate one (this is the same fallback path
/// `somcat` itself falls back to when ITS OWN real ffprobe fails).
const VIDEO_PLACEHOLDER_WIDTH_PX: u32 = 1280;
const VIDEO_PLACEHOLDER_HEIGHT_PX: u32 = 720;

/// Fixed cell footprint for the audio widget — audio has no pixel
/// dimensions of its own to derive one from. Mirrors `somcat::AUDIO_
/// WIDGET_COLUMNS`/`ROWS`.
const AUDIO_WIDGET_COLUMNS: u16 = 40;
const AUDIO_WIDGET_ROWS: u16 = 1;

pub struct Srp;

impl Srp {
	/// Thin public re-export of `SrvChannel::stop_playback` — `SrvChannel`
	/// itself lives in this module's private `srv_channel` submodule, so
	/// `Adapter::stop_previous_media` (in `adapter.rs`, a sibling module)
	/// reaches it through here rather than needing `srv_channel` made
	/// `pub`. See `SrvRequest::StopPlayback`'s own doc comment (in Som's
	/// repo) for the full rationale.
	pub fn stop_playback(session_id: u32, file_id: u32) -> Result<()> {
		srv_channel::SrvChannel::stop_playback(session_id, file_id)
	}

	pub(super) async fn image_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let extension = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
		match extension.as_deref() {
			Some("mp4" | "mkv" | "avi") => Self::stream_video(path, max).await,
			Some("mp3" | "flac") => Self::audio_show(path, max).await,
			_ => Self::show_image(path, max).await,
		}
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

	/// The original image/GIF path — reads the whole (typically small)
	/// file into memory, probes its header, prints the placeholder grid,
	/// then hands the buffer off to `stream_bytes` for chunked sending.
	/// Unlike `somcat`, format is guessed from the file's own magic bytes
	/// (`image::guess_format`) rather than the extension — `path` here is
	/// frequently a hashed cache file yazi itself created with no
	/// extension to check at all.
	async fn show_image(path: PathBuf, max: Rect) -> Result<Rect> {
		let bytes = tokio::fs::read(&path).await?;
		let format = image::guess_format(&bytes)?;
		let content_type = match format {
			image::ImageFormat::Gif => ContentType::Gif,
			image::ImageFormat::Jpeg => ContentType::Jpeg,
			image::ImageFormat::Png => ContentType::Png,
			other => bail!("SRP driver only supports Gif/Jpeg/Png/mp4/mkv/avi/mp3/flac, got {other:?}"),
		};

		let (width_px, height_px, is_animated) = tokio::task::spawn_blocking({
			let bytes = bytes.clone();
			move || metadata::image_dimensions(&bytes, content_type)
		})
		.await??;

		let (session_id, file_id) = new_ids();
		let area = Image::pixel_area((width_px, height_px), max);
		let grid = place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&grid)?;
			Ok(area)
		})?;

		// Fire-and-forget, same reasoning as this driver's video/audio
		// paths below — see `stream_video`'s own doc comment for the full
		// rationale (yazi's preview machinery wants an area back promptly,
		// not "wait for the whole transfer").
		tokio::spawn(async move {
			let content_metadata = ContentMetadata::Image { width_px, height_px, color_bits: 32, is_animated };
			if let Err(err) = stream_bytes_task(bytes, content_type, content_metadata, session_id, file_id).await {
				yazi_macro::error!("[Srp] streaming {path:?} failed: {err}");
			}
		});

		Ok(area)
	}

	/// Streams a video file off disk — never reads the whole file into
	/// memory (real movies are routinely multi-gigabyte) — with a
	/// dedicated byte-range-responder connection so Som's own seeks get
	/// answered promptly. Mirrors `somcat::stream_file`'s video branch:
	/// placeholder grid printed FIRST (Som has no id to open a player
	/// for until it's seen this placement in the grid, confirmed live in
	/// Som's own history as "playback never starts" when this order was
	/// reversed), streaming SECOND, fired on a background task so this
	/// function itself returns promptly with just the reserved area.
	async fn stream_video(path: PathBuf, max: Rect) -> Result<Rect> {
		let total_size = tokio::fs::metadata(&path).await?.len();
		let (width_px, height_px) = (VIDEO_PLACEHOLDER_WIDTH_PX, VIDEO_PLACEHOLDER_HEIGHT_PX);
		let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_string();
		let metadata = ContentMetadata::Video {
			width_px,
			height_px,
			fps_numerator: 0,
			fps_denominator: 0,
			codec: protocol::VideoCodec::Unknown,
			audio_stream_index: None,
			subtitle_stream_index: None,
			extension,
		};

		let (session_id, file_id) = new_ids();
		// One extra row beyond the picture itself, reserved for Som's own
		// play/pause/seek-bar widget — mirrors `somcat::print_video_
		// placeholder_grid`'s `rows + 1` exactly (see that function's own
		// doc comment): Som paints the decoded frame into every row this
		// placement has EXCEPT its last, and the widget into that last
		// row (`terminal_element.rs`'s video branch, "Widget row sits
		// immediately below the picture"). Without this extra row, Som
		// paints the widget row OVER the bottom of the video frame
		// instead of below it.
		//
		// UNLIKE `Image::pixel_area` (used by `show_image` for arbitrary
		// still images, where clamping width/height independently is
		// correct — a picture is free to fill whatever box it's given),
		// this computes the picture's footprint by preserving the video's
		// own `width_px:height_px` aspect ratio (16:9, from the fixed
		// `VIDEO_PLACEHOLDER_WIDTH_PX`/`HEIGHT_PX` constants — see
		// `metadata.rs`'s own doc comment for why real ffprobe dimensions
		// aren't available here) against the pane's available WIDTH only,
		// then deriving height from that ratio — NOT independently
		// clamping height to `picture_max.height` the way `pixel_area`
		// does. A tall/narrow preview pane (portrait-oriented relative to
		// 16:9) would otherwise reserve far MORE height than a letterboxed
		// 16:9 frame actually needs, leaving the widget row stranded near
		// the bottom of the pane with a visible gap above it instead of
		// sitting immediately below the picture — confirmed live.
		let picture_max_height = max.height.saturating_sub(1).max(1);
		let area = match Dimension::cell_size() {
			Some((cw, ch)) => {
				let aspect = height_px as f64 / width_px as f64;
				let picture_columns = (width_px as f64 / cw).ceil() as u16;
				let columns = picture_columns.min(max.width).max(1);
				let pixel_width = columns as f64 * cw;
				let pixel_height = pixel_width * aspect;
				let picture_rows = ((pixel_height / ch).ceil() as u16).min(picture_max_height).max(1);
				Rect { x: max.x, y: max.y, width: columns, height: (picture_rows + 1).min(max.height).max(1) }
			},
			None => max,
		};
		let grid = place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		// See `Adapter::stop_previous_media`'s own doc comment — this
		// records the id so the NEXT `*_show` call (of any content type)
		// tells Som to stop THIS placement's player if it's still open.
		ADAPTOR.record_media_id(session_id, file_id);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&grid)?;
			Ok(area)
		})?;

		tokio::spawn(async move {
			if let Err(err) = stream_file_from_disk_task(path.clone(), ContentType::Video, metadata, session_id, file_id, total_size).await {
				yazi_macro::error!("[Srp] streaming {path:?} failed: {err}");
			}
		});

		Ok(area)
	}

	/// Audio's counterpart to [`Self::show_image`]/`stream_video` — unlike
	/// those, this is `pub` (not `pub(super)`): unlike every other content
	/// type this driver shows, audio has no filesystem-preview-thumbnail
	/// concept in yazi at all, so `Adapter::audio_show`
	/// (`yazi-adapter/src/adapter.rs`) calls straight into this method
	/// rather than going through `Driver::image_show`'s per-driver
	/// dispatch — see that method's own doc comment. Same disk-streaming
	/// path as `stream_video` (never the whole file into memory), but with
	/// real header metadata (`metadata::audio_metadata`, via symphonia) —
	/// unlike video, this is cheap enough (a small header read, not a
	/// multi-gigabyte container probe requiring FFmpeg) to always attempt.
	/// A failed probe falls back to zero-value `ContentMetadata::Audio`
	/// fields (the widget's own footprint, `AUDIO_WIDGET_COLUMNS`/`ROWS`,
	/// is unaffected either way — it's a fixed size, not derived from the
	/// probe), the same "unknown, don't guess" convention `somcat`'s own
	/// `audio_metadata()` uses.
	///
	/// The widget's fixed footprint is centered both horizontally and
	/// vertically within the preview pane, not anchored to its top-left
	/// corner — unlike an image (which naturally fills however much of
	/// the pane its aspect ratio allows), this footprint is small and
	/// fixed-size regardless of how large the pane is, so pinning it to a
	/// corner would leave it looking stranded in a typical (much taller/
	/// wider) pane.
	pub async fn audio_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let total_size = tokio::fs::metadata(&path).await?.len();
		let (sample_rate, channels, bits_per_sample, duration_ms) = tokio::task::spawn_blocking({
			let path = path.clone();
			move || metadata::audio_metadata(&path)
		})
		.await?
		.unwrap_or((0, 0, 0, 0));
		let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_string();
		let content_metadata = ContentMetadata::Audio { sample_rate, channels, bits_per_sample, duration_ms, extension };

		let (session_id, file_id) = new_ids();
		let width = AUDIO_WIDGET_COLUMNS.min(max.width);
		let height = AUDIO_WIDGET_ROWS.min(max.height);
		let area = Rect {
			x: max.x + (max.width.saturating_sub(width)) / 2,
			y: max.y + (max.height.saturating_sub(height)) / 2,
			width,
			height,
		};
		let grid = place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		// See `Adapter::stop_previous_media`'s own doc comment.
		ADAPTOR.record_media_id(session_id, file_id);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&grid)?;
			Ok(area)
		})?;

		tokio::spawn(async move {
			if let Err(err) =
				stream_file_from_disk_task(path.clone(), ContentType::Audio, content_metadata, session_id, file_id, total_size).await
			{
				yazi_macro::error!("[Srp] streaming {path:?} failed: {err}");
			}
		});

		Ok(area)
	}

	/// Markdown's counterpart to [`Self::audio_show`] — same "bypass the
	/// per-driver `Driver::image_show` dispatch, call `Srp` directly"
	/// shape, for the same reason: markdown rendering (parsing/layout/
	/// styling) happens entirely on Som's OWN receiving end (see Som's
	/// `terminal_view::markdown_styling`/`paint_rich_content_markdown_
	/// widget`), not in this process — this function's only job is to
	/// reserve a placeholder footprint and stream the raw `.md` bytes,
	/// mirroring `somcat::print_markdown_placeholder_grid`/`stream_bytes`
	/// exactly. Deliberately NATIVE-MARKDOWN-ONLY: no Lua (`@@@`/`@[...]`
	/// markers) or form (`$[...]`/`$$$`) syntax recognition belongs here,
	/// or anywhere in this driver — those are still just design notes, not
	/// implemented on Som's receiving side either, and this driver's whole
	/// job is to mirror what `somcat` already does today, not anticipate
	/// features neither side has built yet.
	pub async fn markdown_show(path: PathBuf, max: Rect) -> Result<Rect> {
		let bytes = tokio::fs::read(&path).await?;
		let content_metadata = ContentMetadata::Markdown;

		let (session_id, file_id) = new_ids();
		// UNLIKE `somcat::print_markdown_placeholder_grid` (which reserves
		// the document's FULL height, relying on the real terminal's own
		// scrollback for anything taller than the viewport), this grid is
		// clamped to `max.height` — yazi's preview pane, unlike a real
		// shell prompt, has no scrollback of its own to hold an oversized
		// placement in; a grid taller than `max` here doesn't overflow
		// into anything readable, it just mismatches yazi's own `LAYOUT.
		// preview` rect on the very next render tick (`Preview::render` in
		// `yazi-fm/src/mgr/preview.rs` skips rendering entirely when the
		// placed area disagrees with the current layout), which is what
		// made the ENTIRE 3rd (preview) column vanish, not just the
		// markdown content — confirmed live: a 354-line `markdown.md`
		// against a 48-row-tall preview pane produced a 267-row grid,
		// and the column disappeared on the next frame. Width was already
		// correctly clamped to `max.width`; height needs the same
		// treatment.
		//
		// The RENDERED row count (`markdown_line_count::count_rendered_
		// lines`), not the raw file's newline count — markdown source
		// formatting (extra blank lines, list item density, raw HTML
		// blocks that render to nothing, etc.) doesn't map 1:1 onto
		// rendered rows, so reserving `bytes`' own `\n` count left a real,
		// visible gap between the widget's actual painted content and
		// whatever came after it in the pane — the same bug found and
		// fixed in `somcat::print_markdown_placeholder_grid` (Som's own
		// repo), ported here so both clients reserve identical footprints
		// for the same document.
		let source = String::from_utf8_lossy(&bytes);
		let line_count = markdown_line_count::count_rendered_lines(&source).max(1);
		let width = max.width.max(1);
		let height = line_count.min(max.height as u32).max(1) as u16;
		let area = Rect { x: max.x, y: max.y, width, height };
		let grid = place(&area, session_id, file_id)?;

		ADAPTOR.image_hide()?;
		ADAPTOR.shown_store(area);
		Emulator::move_lock((area.x, area.y), |w| {
			w.write_all(&grid)?;
			Ok(area)
		})?;

		let content_type = ContentType::Markdown;
		tokio::spawn(async move {
			if let Err(err) = stream_bytes_task(bytes, content_type, content_metadata, session_id, file_id).await {
				yazi_macro::error!("[Srp] streaming {path:?} failed: {err}");
			}
		});

		Ok(area)
	}
}

/// Derives a fresh `(session_id, file_id)` pair — time-derived and masked
/// to 24 bits, mirrors `somcat::new_ids` exactly (both are painted into a
/// placeholder cell's RGB color, which only has 24 bits to work with).
fn new_ids() -> (u32, u32) {
	let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(1);
	let session_id = (now_ms & 0xFF_FFFF).max(1);
	let file_id = (now_ms.wrapping_mul(2_654_435_761) & 0xFF_FFFF).max(1);
	(session_id, file_id)
}

/// Prints the Unicode-placeholder grid text for `area`, encoding
/// `session_id`/`file_id` into the foreground/underline color of every
/// cell — mirrors `somcat::print_placeholder_grid`'s own byte layout
/// exactly (Som's receiving-side parser doesn't know or care which
/// process painted these cells).
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

/// Foreground carries session_id, underline color carries file_id — same
/// split Som's own `somcat` client uses.
fn id_to_rgb(id: u32) -> (u8, u8, u8) { (((id >> 16) & 0xFF) as u8, ((id >> 8) & 0xFF) as u8, (id & 0xFF) as u8) }

/// Sends the whole in-memory `bytes` buffer as `CHUNK_SIZE`-sized
/// `PutChunk`s over a fresh connection — the image/GIF path's transfer
/// implementation, run via `spawn_blocking` since it's a tight loop of
/// synchronous `SrvChannel::put_chunk` calls (this driver's `SrvChannel`
/// is plain blocking I/O, mirroring `somcat`'s own, not async).
async fn stream_bytes_task(bytes: Vec<u8>, content_type: ContentType, metadata: ContentMetadata, session_id: u32, file_id: u32) -> Result<()> {
	tokio::task::spawn_blocking(move || -> Result<()> {
		let channel = SrvChannel::connect()?;
		let total_size = bytes.len() as u64;
		let mut offset = 0u64;
		for chunk in bytes.chunks(CHUNK_SIZE) {
			channel.put_chunk(session_id, file_id, offset, chunk.to_vec(), total_size, content_type, metadata.clone())?;
			offset += chunk.len() as u64;
		}
		Ok(())
	})
	.await?
}

/// How much of the file's own FRONT to push proactively before settling
/// into pure pull/responder mode — mirrors `somcat::ANNOUNCE_LEN` exactly.
/// Deliberately small and bounded: just enough for `som-srv` to create
/// its cache entry (`total_size`/`content_type`/`metadata`, needed
/// before Som has anything to subscribe TO at all) and for Som's own
/// decoder to have a real shot at probing the format without a round
/// trip. Everything past this (including the rest of the front region,
/// if the probe needs more) arrives only via `RequestByteRange`.
const ANNOUNCE_LEN: u64 = 256 * 1024;

/// Video-only speculative tail fetch, alongside the front announce —
/// mirrors `somcat::TAIL_FETCH_LEN` exactly. Non-faststart MP4 (`moov`
/// atom at the end) and MKV with Cues near the end both need this: Som's
/// `SeekFrom::End`-derived probe would otherwise need a full extra
/// `RequestByteRange` round trip just to learn the tail even exists.
const TAIL_FETCH_LEN: u64 = 4 * 1024 * 1024;

/// Streams `path` off disk for video/audio PULL-STYLE — mirrors
/// `somcat::stream_file_from_disk` exactly: send a bounded front announce
/// (`ANNOUNCE_LEN`) plus, for video, a bounded tail fetch (`TAIL_FETCH_
/// LEN`), then register as the byte-range responder and answer `Request
/// ByteRange` on demand for as long as this task's own responder thread
/// keeps running — no more proactive sequential push past the initial
/// announce/tail. This REPLACES an earlier "sequential push, `Request
/// ByteRange` only as a fallback for stragglers" design (superseded in
/// Som's own `somcat` first, ported here to match) — that design raced a
/// fast local transfer finishing before Som had even subscribed, leaving
/// nothing left to answer a stuck reader with on a large file.
///
/// UNLIKE `somcat` (a one-shot process that blocks in the foreground
/// until playback ends, then exits), this is a `tokio::spawn`ed
/// background task inside yazi's own long-running process — there is no
/// "hold the terminal" concern here, so this function simply returns
/// once the responder thread itself exits (on `EndPlayback`, a channel
/// read error, or `stop_previous_media`'s own `StopPlayback` tearing the
/// OLD placement down when the preview cursor moves to a different
/// file — see `Adapter::stop_previous_media`'s own doc comment). The file
/// handle and responder connection are released the same way either way.
async fn stream_file_from_disk_task(
	path: PathBuf,
	content_type: ContentType,
	metadata: ContentMetadata,
	session_id: u32,
	file_id: u32,
	total_size: u64,
) -> Result<()> {
	tokio::task::spawn_blocking(move || -> Result<()> {
		let file = std::fs::File::open(&path)?;
		let shared_file = Arc::new(std::sync::Mutex::new(file));
		let channel = SrvChannel::connect()?;

		let announce_len = ANNOUNCE_LEN.min(total_size);
		send_range_chunks(&channel, &shared_file, content_type, metadata.clone(), session_id, file_id, total_size, 0, announce_len)?;

		if content_type == ContentType::Video {
			let tail_offset = total_size.saturating_sub(TAIL_FETCH_LEN);
			let _ = send_range_chunks(
				&channel,
				&shared_file,
				content_type,
				metadata.clone(),
				session_id,
				file_id,
				total_size,
				tail_offset,
				TAIL_FETCH_LEN,
			);
		}

		let (_stop, handle) = spawn_byte_range_responder(shared_file, content_type, metadata, session_id, file_id, total_size)?;
		let _ = handle.join();
		Ok(())
	})
	.await?
}

/// Registers a SECOND connection as the dedicated target for `Request
/// ByteRange` forwarding, then loops answering forwarded range requests
/// on demand — mirrors `somcat::spawn_byte_range_responder_from_disk`
/// exactly, including reacting to `SrvRequest::EndPlayback` by exiting
/// the loop instead of answering it (Som's signal that playback has
/// definitively ended for this key, so there is nothing left to serve).
#[allow(clippy::type_complexity)]
fn spawn_byte_range_responder(
	file: Arc<std::sync::Mutex<std::fs::File>>,
	content_type: ContentType,
	metadata: ContentMetadata,
	session_id: u32,
	file_id: u32,
	total_size: u64,
) -> Result<(Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
	let channel = Arc::new(SrvChannel::connect()?);
	channel.register_range_responder(session_id, file_id)?;

	let stop = Arc::new(AtomicBool::new(false));
	let stop_for_thread = stop.clone();
	let handle = std::thread::spawn(move || {
		loop {
			if stop_for_thread.load(Ordering::Relaxed) {
				return;
			}
			match channel.read_incoming() {
				Ok(srv_channel::Incoming::Request(protocol::SrvRequest::RequestByteRange {
					session_id: req_session,
					file_id: req_file,
					offset,
					len,
				})) if (req_session, req_file) == (session_id, file_id) => {
					let _ = send_range_chunks(&channel, &file, content_type, metadata.clone(), session_id, file_id, total_size, offset, len);
				},
				Ok(srv_channel::Incoming::Request(protocol::SrvRequest::EndPlayback {
					session_id: req_session,
					file_id: req_file,
				})) if (req_session, req_file) == (session_id, file_id) => {
					return;
				},
				Ok(_) => continue,
				Err(_) => return,
			}
		}
	});
	Ok((stop, handle))
}

/// Sends `[range_offset, range_offset + range_len)` as `CHUNK_SIZE`-sized
/// `PutChunk`s — mirrors `somcat::send_range_chunks_from_disk` exactly.
/// No seek-redirect logic (unlike the old, superseded interruptible
/// version this replaces): the pull model has nothing to interrupt — a
/// sender never has a stale in-flight sequential send to redirect away
/// from, since it only ever sends exactly what was requested.
#[allow(clippy::too_many_arguments)]
fn send_range_chunks(
	channel: &SrvChannel,
	file: &std::sync::Mutex<std::fs::File>,
	content_type: ContentType,
	metadata: ContentMetadata,
	session_id: u32,
	file_id: u32,
	total_size: u64,
	range_offset: u64,
	range_len: u64,
) -> Result<()> {
	use std::io::{Read as _, Seek as _, SeekFrom};

	let range_len = range_len.min(total_size.saturating_sub(range_offset));
	let mut offset = range_offset;
	let end = range_offset + range_len;
	let mut buf = vec![0u8; CHUNK_SIZE];
	while offset < end {
		let piece_len = (end - offset).min(CHUNK_SIZE as u64) as usize;
		{
			let mut guard = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
			guard.seek(SeekFrom::Start(offset))?;
			guard.read_exact(&mut buf[..piece_len])?;
		}
		channel.put_chunk(session_id, file_id, offset, buf[..piece_len].to_vec(), total_size, content_type, metadata.clone())?;
		offset += piece_len as u64;
	}
	Ok(())
}

/// Real-transport integration tests — require a live `som-srv` daemon
/// already reachable at `~/.local/bin/som-srv[.exe]` or already running
/// (`daemon::connect_or_spawn` will spawn it from that fixed path if not).
/// These exercise the exact code paths `Srp::image_show`/`stream_video`/
/// `stream_audio` hand off to on a background task — connect, handshake,
/// `PutChunk` framing, and (for video/audio) the dedicated byte-range-
/// responder registration — without needing a real TTY/`Emulator`
/// context, which the `image_show`/`stream_video`/`stream_audio`
/// entry points themselves require and can't be exercised headless.
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stream_bytes_task_delivers_a_real_gif_over_a_real_som_srv_connection() {
		// Points at Som's own repo fixtures rather than duplicating them
		// into this fork — the two live as sibling checkouts on this dev
		// machine (`C:\home\dnk\som`, `C:\home\dnk\yazi-fork-work`), which
		// this hardcodes; skips cleanly if that layout doesn't hold (e.g.
		// CI, a different machine).
		let path = std::path::PathBuf::from(r"C:\home\dnk\som\giphy.gif");
		let Ok(bytes) = std::fs::read(&path) else {
			eprintln!("skipping: {} not present", path.display());
			return;
		};

		let rt = tokio::runtime::Runtime::new().unwrap();
		let (session_id, file_id) = new_ids();
		let metadata = ContentMetadata::Image { width_px: 1, height_px: 1, color_bits: 32, is_animated: true };
		let result = rt.block_on(stream_bytes_task(bytes, ContentType::Gif, metadata, session_id, file_id));

		assert!(result.is_ok(), "streaming a real GIF over a live som-srv connection failed: {result:?}");
	}

	/// Since the pull-model redesign, `stream_file_from_disk_task` sends a
	/// bounded announce/tail then blocks in its own responder thread until
	/// `SrvRequest::EndPlayback` arrives (or the connection errors) — see
	/// that function's own doc comment. A test exercising it therefore
	/// needs to send `EndPlayback` itself (standing in for Som, via
	/// `SrvChannel::end_playback_for_test`) to ever let the task return,
	/// instead of just awaiting it directly the way the OLD sequential-
	/// push design's test used to.
	#[test]
	fn stream_file_from_disk_task_delivers_a_real_video_over_a_real_som_srv_connection() {
		let path = std::path::PathBuf::from(r"C:\home\dnk\som\sample_1920x1080.mkv");
		let Ok(total_size) = std::fs::metadata(&path).map(|m| m.len()) else {
			eprintln!("skipping: {} not present", path.display());
			return;
		};

		let rt = tokio::runtime::Runtime::new().unwrap();
		let (session_id, file_id) = new_ids();
		let metadata = ContentMetadata::Video {
			width_px: VIDEO_PLACEHOLDER_WIDTH_PX,
			height_px: VIDEO_PLACEHOLDER_HEIGHT_PX,
			fps_numerator: 0,
			fps_denominator: 0,
			codec: protocol::VideoCodec::Unknown,
			audio_stream_index: None,
			subtitle_stream_index: None,
			extension: "mkv".to_string(),
		};
		let task = rt.spawn(stream_file_from_disk_task(path.clone(), ContentType::Video, metadata, session_id, file_id, total_size));

		// Send `EndPlayback` repeatedly on a short interval, in the
		// background, until the task itself exits — a SUCCESSFUL send
		// only means the daemon accepted the message, not that it was
		// actually delivered to a registered range-responder route (the
		// task hasn't necessarily called `RegisterRangeResponder` yet,
		// since it sends its announce chunk and — for video — a bounded
		// tail fetch first; see `stream_file_from_disk_task`'s own doc
		// comment). A single fixed-delay attempt is therefore unreliable;
		// retrying is what actually guarantees delivery once the route
		// exists, without needing to reach into the task's own internal
		// state from this test.
		let stop_retrying = Arc::new(AtomicBool::new(false));
		let stop_retrying_for_thread = stop_retrying.clone();
		let retry_handle = std::thread::spawn(move || {
			while !stop_retrying_for_thread.load(Ordering::Relaxed) {
				let _ = SrvChannel::end_playback_for_test(session_id, file_id);
				std::thread::sleep(std::time::Duration::from_millis(200));
			}
		});

		let result = rt.block_on(async {
			tokio::time::timeout(std::time::Duration::from_secs(15), task).await.expect("task did not exit after EndPlayback")
		});
		stop_retrying.store(true, Ordering::Relaxed);
		let _ = retry_handle.join();

		assert!(result.is_ok(), "streaming a real video file over a live som-srv connection failed: {result:?}");
	}
}
