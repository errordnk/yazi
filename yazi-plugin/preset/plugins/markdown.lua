-- Markdown previewer — SRP-only (Som's Rich Protocol). Parsing/layout/
-- styling of the `.md` document all happen entirely on Som's own
-- receiving end (see `yazi-adapter/src/drivers/srp/mod.rs`'s
-- `Srp::markdown_show` doc comment, and Som's own `terminal_view::
-- markdown_styling` for the render side), not in this process — this
-- previewer's only job is to stream the raw file to Som via SRP if
-- we're actually running inside it, or fall back to `code.lua`'s plain
-- syntax-highlighted text preview everywhere else, rather than silently
-- doing nothing. Deliberately native-markdown-only, same as Som's own
-- receiving-side renderer: no Lua/form syntax recognition belongs on
-- either side of this pipeline yet.
local M = {}

-- `SOM_WINDOW_ID` is Som's own capability signal — see
-- SRP_INTEGRATION_GUIDE.md's "Detecting that you're running inside Som"
-- section. Checked here (Lua side), not left to `ya.markdown_show`'s own
-- Rust-side driver check, so a non-Som terminal gets the useful
-- `code.lua` fallback instead of a raw error surfaced through
-- `ya.preview_widget`.
local function inside_som() return os.getenv("SOM_WINDOW_ID") ~= nil end

function M:peek(job)
	if not inside_som() then
		return require("code"):peek(job)
	end

	local _, err = ya.markdown_show(job.file.url, job.area)
	ya.preview_widget(job, err)
end

function M:seek() end

function M:spot(job) return require("code"):spot(job) end

return M
