//! Header probing for the SRP driver — width/height (plus a cheap
//! animation check for GIF) for images, and sample_rate/channels/bits_
//! per_sample/duration_ms for audio, without decoding any actual pixel/
//! sample data. Mirrors `somcat`'s own `gif_metadata`/`static_image_
//! metadata`/`audio_metadata`.
//!
//! Deliberately does NOT probe VIDEO the way `somcat` does (FFmpeg/
//! `ffmpeg-next` for width/height/fps/codec) — that's a large, platform-
//! specific dependency (FFmpeg needs a set of DLLs deployed and DLL-
//! search-path wiring on Windows, see `terminal::rich_content_video_
//! player::ensure_ffmpeg_extracted_and_wired`'s doc comment in Som for
//! the extent of that machinery) that doesn't belong in a general-
//! purpose file manager's adapter crate just to reserve an accurate
//! placeholder footprint. Video instead falls back to a fixed
//! placeholder size (`super::VIDEO_PLACEHOLDER_WIDTH_PX`/`HEIGHT_PX`) —
//! the exact same fallback `somcat` itself uses when its own real probe
//! fails — Som decodes the real file and learns its true dimensions
//! once playback actually starts regardless of what this driver
//! reserved ahead of time. Audio's `symphonia` probe, by contrast, is
//! cheap (a small header read, not a multi-gigabyte container probe)
//! and already a dependency this crate carries for the audio previewer,
//! so it's always attempted — see `audio_metadata`'s own doc comment.

use anyhow::Result;
use image::ImageDecoder as _;

use super::protocol::ContentType;

pub(super) fn image_dimensions(bytes: &[u8], content_type: ContentType) -> Result<(u32, u32, bool)> {
	if content_type == ContentType::Gif {
		let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))?;
		let (width, height) = decoder.dimensions();
		// A second Image Descriptor block (0x2C) before EOF means more than
		// one frame — cheap animation check without decoding any frame,
		// mirrors somcat::gif_metadata's own heuristic exactly.
		let is_animated = bytes.windows(1).skip(13).filter(|w| w[0] == 0x2C).count() > 1;
		Ok((width, height, is_animated))
	} else {
		let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
		let (width, height) = reader.into_dimensions()?;
		Ok((width, height, false))
	}
}

/// Probes `sample_rate`/`channels`/`bits_per_sample`/`duration_ms` from
/// the file's own container header via `symphonia`'s format reader — no
/// full decode, mirroring `somcat`'s own `audio_metadata()`.
/// `duration_ms` comes from `CodecParameters::n_frames`/`time_base` (an
/// MP3's Xing/VBRI VBR tag or size estimate, a FLAC's STREAMINFO block)
/// — `0` if the format reader couldn't determine it, same "unknown,
/// don't guess" convention every other SRP metadata field uses.
pub(super) fn audio_metadata(path: &std::path::Path) -> anyhow::Result<(u32, u8, u8, u32)> {
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
	let channels = params.channels.map(|c| c.count() as u8).ok_or_else(|| anyhow::anyhow!("{path:?}: unknown channel count"))?;
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
