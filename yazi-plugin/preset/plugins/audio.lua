-- Audio previewer — SRP-only (Som's Rich Protocol). Every other terminal
-- graphics protocol this project supports (Kgp/KgpOld/Iip/Sixel/Chafa/
-- Ueberzug) has no audio concept at all: they exist to place PIXELS on
-- screen, and audio decoding/playback in this codebase happens entirely
-- on Som's own receiving end, not in this process (see
-- `yazi-adapter/src/drivers/srp.rs`'s `Srp::audio_show` doc comment, and
-- SRP_INTEGRATION_GUIDE.md's "Audio: streamed progressively" section in
-- the Som repository for the full rationale). So this previewer only
-- ever does one of two things: stream the file to Som via SRP if we're
-- actually running inside it, or fall back to `file.lua`'s plain
-- classification preview everywhere else, rather than silently doing
-- nothing.
local M = {}

-- `SOM_WINDOW_ID` is Som's own capability signal — see
-- SRP_INTEGRATION_GUIDE.md's "Detecting that you're running inside Som"
-- section. Checked here (Lua side), not left to `ya.audio_show`'s own
-- Rust-side driver check, so a non-Som terminal gets the useful
-- `file.lua` fallback instead of a raw error surfaced through
-- `ya.preview_widget`.
local function inside_som() return os.getenv("SOM_WINDOW_ID") ~= nil end

function M:peek(job)
	if not inside_som() then
		return require("file"):peek(job)
	end

	local _, err = ya.audio_show(job.file.url, job.area)
	ya.preview_widget(job, err)
end

function M:seek() end

function M:spot(job) return require("file"):spot(job) end

return M
