//! Client wrapper over `som-srv`'s binary side channel — mirrors Som's own
//! `crates/somcat/src/srv_channel.rs` field-for-field and method-for-
//! method (see that file for the full rationale behind each design
//! choice; this is a direct port against this driver's own `pipe`/
//! `protocol`/`daemon` ports rather than a shared dependency — see
//! `protocol.rs`'s own doc comment for why).

use super::daemon;
use super::pipe::PipeConnection;
use super::protocol::{ConnectionKind, HandshakeInfo, SrvRequest, SrvResponse};

#[derive(Debug)]
pub enum Incoming {
	Response(SrvResponse),
	Request(SrvRequest),
}

pub struct SrvChannel {
	connection: PipeConnection,
	// Guards `write_message` — `PutChunk`s can arrive from two different
	// OS threads on the SAME connection in principle (this driver's own
	// sequential sender never shares a connection with its byte-range
	// responder — see `RegisterRangeResponder`'s own doc comment for
	// why — but kept for the same defense-in-depth reason `somcat`'s
	// copy has it: `write_message` itself does two separate `write_all`
	// calls with no internal locking, and any future caller adding a
	// second writer on this connection would otherwise silently
	// reintroduce the exact interleaved-framing bug `somcat`'s own
	// comment describes).
	write_lock: std::sync::Mutex<()>,
}

impl SrvChannel {
	pub fn connect() -> anyhow::Result<Self> {
		let connection = daemon::connect_or_spawn()?;
		ConnectionKind::Srv.write_to(&connection)?;

		send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
		match read_any(&connection)? {
			Incoming::Response(SrvResponse::Handshake(_)) => {},
			other => anyhow::bail!("expected Handshake as som-srv's first reply, got {other:?}"),
		}

		Ok(Self { connection, write_lock: std::sync::Mutex::new(()) })
	}

	#[allow(clippy::too_many_arguments)]
	pub fn put_chunk(
		&self,
		session_id: u32,
		file_id: u32,
		offset: u64,
		data: Vec<u8>,
		total_size: u64,
		content_type: super::protocol::ContentType,
		metadata: super::protocol::ContentMetadata,
	) -> anyhow::Result<()> {
		let _guard = self.write_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		send(&self.connection, &SrvRequest::PutChunk { session_id, file_id, offset, data, total_size, content_type, metadata })
	}

	pub fn read_incoming(&self) -> anyhow::Result<Incoming> { read_any(&self.connection) }

	pub fn register_range_responder(&self, session_id: u32, file_id: u32) -> anyhow::Result<()> {
		send(&self.connection, &SrvRequest::RegisterRangeResponder { session_id, file_id })
	}

	/// Sends `SrvRequest::StopPlayback` — see that variant's own doc
	/// comment. A fresh one-shot connection (mirroring `RequestByteRange`'s
	/// own shape on Som's side): whatever connection is currently
	/// streaming/responding for `(session_id, file_id)` has nothing to do
	/// with delivering this, since it's forwarded to SOM's subscription
	/// connection, not back to the sender.
	pub fn stop_playback(session_id: u32, file_id: u32) -> anyhow::Result<()> {
		let connection = daemon::connect_or_spawn()?;
		ConnectionKind::Srv.write_to(&connection)?;
		send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
		match read_any(&connection)? {
			Incoming::Response(SrvResponse::Handshake(_)) => {},
			other => anyhow::bail!("expected Handshake as som-srv's first reply, got {other:?}"),
		}
		send(&connection, &SrvRequest::StopPlayback { session_id, file_id })
	}

	/// Test-only: sends `SrvRequest::EndPlayback` directly, standing in
	/// for Som (the real, only legitimate sender of this message) so a
	/// headless test can unblock `spawn_byte_range_responder`'s read loop
	/// without a live Som window. Same fresh-one-shot-connection shape as
	/// `stop_playback` above — the daemon routes this to whichever
	/// connection registered as the range responder for `(session_id,
	/// file_id)`, exactly like it would for a real Som-originated one.
	#[cfg(test)]
	pub fn end_playback_for_test(session_id: u32, file_id: u32) -> anyhow::Result<()> {
		let connection = daemon::connect_or_spawn()?;
		ConnectionKind::Srv.write_to(&connection)?;
		send(&connection, &SrvRequest::Handshake(HandshakeInfo::current()))?;
		match read_any(&connection)? {
			Incoming::Response(SrvResponse::Handshake(_)) => {},
			other => anyhow::bail!("expected Handshake as som-srv's first reply, got {other:?}"),
		}
		send(&connection, &SrvRequest::EndPlayback { session_id, file_id })
	}
}

fn send(connection: &PipeConnection, message: &SrvRequest) -> anyhow::Result<()> {
	let payload = serde_json::to_vec(message)?;
	connection.write_message(&payload)
}

fn read_any(connection: &PipeConnection) -> anyhow::Result<Incoming> {
	let message = connection.read_message()?;
	if let Ok(response) = serde_json::from_slice::<SrvResponse>(&message) {
		return Ok(Incoming::Response(response));
	}
	serde_json::from_slice::<SrvRequest>(&message)
		.map(Incoming::Request)
		.map_err(|err| anyhow::anyhow!("failed to decode message from som-srv as either SrvResponse or SrvRequest: {err}"))
}
