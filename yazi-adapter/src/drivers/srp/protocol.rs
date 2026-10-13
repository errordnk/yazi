//! Minimal port of `som_srv::protocol` — just the pieces an SRP CLIENT
//! (this driver, never the daemon) actually needs: `Handshake`, `PutChunk`,
//! `RequestByteRange`/`RegisterRangeResponder`, and the `ContentType`/
//! `ContentMetadata`/`VideoCodec` enums those messages carry. Ported by
//! hand rather than depending on the `som_srv` crate directly, because
//! `som_srv` pulls in `alacritty_terminal`/`smol`/`sysinfo`/`zlog` — all
//! Som-internal crates with no reason to exist inside a general-purpose
//! file manager's dependency tree (see Som's own `crates/som_srv/Cargo.toml`
//! for the full list this driver deliberately avoids). Field layouts here
//! MUST stay in sync with `som_srv::protocol` by hand — both sides
//! communicate over `serde_json`, so a mismatched field name or variant
//! order breaks wire compatibility silently (a renamed/reordered enum
//! variant still round-trips through `serde`'s default representation,
//! just as a DIFFERENT variant than intended).
//!
//! Deliberately omits every session-management/relay variant
//! (`RelayInput`, `HolderOutput`, `ListSessions`, `KillSession`,
//! `SubscribeProgress`, `SrvResponse::Sessions`/`Killed`/`Progress`) — this
//! driver only ever sends `PutChunk`s and answers `RequestByteRange`, the
//! same restricted subset `somcat` itself uses for a pure sender role (see
//! Som's `crates/somcat/src/srv_channel.rs`, which this module mirrors).

use serde::{Deserialize, Serialize};

/// Same fixed address `som_srv::protocol::daemon_socket_path` uses — MUST
/// match exactly, since this is how a completely independent process
/// (this driver, running inside `ya`/`yazi`) finds the same daemon Som's
/// own `somcat`/`crates/terminal` clients connect to.
#[cfg(windows)]
pub fn daemon_socket_path() -> String { r"\\.\pipe\somsrv".to_string() }

#[cfg(unix)]
pub fn daemon_socket_path() -> String {
	let dir = std::path::Path::new("/tmp");
	let _ = std::fs::create_dir_all(dir);
	dir.join(format!("somsrv-{}.sock", unsafe { libc::getuid() })).to_string_lossy().into_owned()
}

/// Mirrors `som_srv::protocol::Os` — part of `HandshakeInfo`, which the
/// daemon never actually rejects a client over (see that type's own doc
/// comment in Som), but the field must still round-trip through `serde`
/// correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
	Windows,
	Darwin,
	Linux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
	Amd64,
	Arm64,
}

fn current_platform() -> (Os, Arch) {
	let os = if cfg!(target_os = "windows") {
		Os::Windows
	} else if cfg!(target_os = "macos") {
		Os::Darwin
	} else {
		Os::Linux
	};
	let arch = if cfg!(target_arch = "x86_64") { Arch::Amd64 } else { Arch::Arm64 };
	(os, arch)
}

/// Mirrors `som_srv::protocol::HandshakeInfo` — `version` here is THIS
/// driver's own build version (yazi's, not Som's or som-srv's); the
/// daemon only logs/ignores a mismatch for a pure `Srv`-kind client, it
/// never refuses the connection over it (confirmed by reading
/// `som_srv::server`'s handshake handling — version compatibility only
/// gates RELAY reattachment, a concept this driver never participates
/// in).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeInfo {
	pub version: String,
	pub os: Os,
	pub arch: Arch,
}

impl HandshakeInfo {
	pub fn current() -> Self {
		let (os, arch) = current_platform();
		Self { version: env!("CARGO_PKG_VERSION").to_string(), os, arch }
	}
}

/// Mirrors `som_srv::protocol::ConnectionKind` — the one leading byte
/// every fresh connection to the daemon must send before anything else,
/// picking which protocol (`RelayInput`/`HolderOutput` PTY proxying, or
/// this module's `SrvRequest`/`SrvResponse` binary side-channel) the rest
/// of the connection speaks. This driver only ever sends `Srv` (`1`) —
/// never `Relay` (`0`), which is exclusively som-srv's PTY-relay role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectionKind {
	Srv = 1,
}

impl ConnectionKind {
	pub fn write_to(self, conn: &super::pipe::PipeConnection) -> anyhow::Result<()> {
		conn.write_message(&[self as u8])
	}
}

/// Mirrors `som_srv::protocol::ContentType` field-for-field — see that
/// type's own doc comment for why this is a hand-kept copy, not a shared
/// dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentType {
	Gif,
	Audio,
	Markdown,
	Video,
	Jpeg,
	Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
	Unknown,
	H264,
	H265,
	Vp9,
	Av1,
	Mpeg4,
}

/// Mirrors `som_srv::protocol::ContentMetadata` field-for-field.
///
/// `extension` (Audio/Video only) is the original file extension without
/// the leading dot (e.g. `"mp4"`, `"mkv"`) — som-srv never decodes media
/// itself, so this is the only way the Som-side decoder learns the real
/// container format once there's no on-disk path left to inspect. Must
/// stay a plain `String` (not `Copy`) — see the `Clone`-only derive here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentMetadata {
	Image { width_px: u32, height_px: u32, color_bits: u8, is_animated: bool },
	Audio { sample_rate: u32, channels: u8, bits_per_sample: u8, duration_ms: u32, extension: String },
	Video {
		width_px: u32,
		height_px: u32,
		fps_numerator: u32,
		fps_denominator: u32,
		codec: VideoCodec,
		audio_stream_index: Option<u32>,
		subtitle_stream_index: Option<u32>,
		extension: String,
	},
	Markdown,
}

/// Mirrors the CLIENT-relevant subset of `som_srv::protocol::SrvRequest`
/// — see this module's own doc comment for which variants are
/// deliberately missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SrvRequest {
	Handshake(HandshakeInfo),
	#[allow(clippy::too_many_arguments)]
	PutChunk {
		session_id: u32,
		file_id: u32,
		offset: u64,
		data: Vec<u8>,
		total_size: u64,
		content_type: ContentType,
		metadata: ContentMetadata,
	},
	/// Forwarded verbatim by the daemon to whichever connection is
	/// registered to answer for `(session_id, file_id)` — see
	/// `som_srv::protocol::SrvRequest::RequestByteRange`'s own doc
	/// comment. This driver only ever RECEIVES this variant (as an
	/// `Incoming::Request`), never sends it — sending it is Som's job.
	RequestByteRange { session_id: u32, file_id: u32, offset: u64, len: u64 },
	/// Sent on a dedicated second connection to become the target for
	/// `RequestByteRange` forwarding, instead of whichever connection is
	/// sending the sequential `PutChunk` stream — see
	/// `som_srv::protocol::SrvRequest::RegisterRangeResponder`'s own doc
	/// comment for why a shared connection would otherwise starve seek
	/// responses behind mutex contention on a large in-flight transfer.
	RegisterRangeResponder { session_id: u32, file_id: u32 },
	/// Sent on a fresh, one-shot connection — the reverse direction of
	/// `RequestByteRange`: this driver SENDS this, never receives it (Som
	/// is always the one subscribed and listening for it). See
	/// `som_srv::protocol::SrvRequest::StopPlayback`'s own doc comment in
	/// Som's repo for the full rationale — tells Som to tear down an
	/// audio/video placement's player when yazi's preview cursor moves to
	/// a different file, since overwriting the old placement's
	/// placeholder cells alone leaves the old player decoding/playing
	/// with no widget left to reach it.
	StopPlayback { session_id: u32, file_id: u32 },
	/// Forwarded verbatim by the daemon to whichever connection is
	/// registered as the range responder for `(session_id, file_id)` —
	/// the REVERSE direction of `StopPlayback` above: Som sends this,
	/// telling THIS driver playback has definitively ended (natural EOF
	/// or the widget's own stop icon), so the responder thread serving
	/// that key should stop. This driver only ever RECEIVES this variant
	/// (as an `Incoming::Request`), never sends it. See `som_srv::
	/// protocol::SrvRequest::EndPlayback`'s own doc comment in Som's repo
	/// for the full rationale.
	EndPlayback { session_id: u32, file_id: u32 },
}

/// Mirrors the CLIENT-relevant subset of `som_srv::protocol::SrvResponse`
/// — this driver never subscribes to `Progress` (that's Som's own job,
/// via `crates/terminal`'s `rich_content_srv_channel`) and never issues
/// `ListSessions`/`KillSession`, so `Sessions`/`Killed`/`Progress` are all
/// omitted; `read_any` (see `srv_channel.rs`) only ever needs to
/// recognize `Handshake` here, falling back to `SrvRequest` for anything
/// else the daemon forwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SrvResponse {
	Handshake(HandshakeInfo),
}
