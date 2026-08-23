local M = {}

function M:peek(job)
	local start = os.clock()
	local url = Url(job.file.path)

	-- The precache below (see `preload`) re-encodes to a single static
	-- PNG/JPEG frame, which is correct for a still image but silently
	-- throws away every frame after the first for a GIF — SRP's own
	-- driver plays the animation itself once Som has the real file, so a
	-- GIF must always go through as the original file, never the cache,
	-- regardless of whether a cache entry already exists.
	if tostring(job.file.url.ext or ""):lower() ~= "gif" then
		local cache = ya.file_cache(job)
		if cache and fs.cha(cache) then
			url = cache
		end
	end

	ya.sleep(math.max(0, rt.preview.image_delay / 1000 + start - os.clock()))

	local _, err = ya.image_show(url, job.area)
	ya.preview_widget(job, err)
end

function M:seek() end

function M:preload(job)
	local cache = ya.file_cache(job)
	if not cache or fs.cha(cache) then
		return true
	end

	return ya.image_precache(Url(job.file.path), cache)
end

function M:spot(job)
	local rows = self:spot_base(job)
	rows[#rows + 1] = ui.Row {}

	ya.spot_table(
		job,
		ui.Table(ya.list_merge(rows, require("file"):spot_base(job)))
			:area(ui.Pos { "center", w = 60, h = 20 })
			:row(job.skip)
			:row(1)
			:col(1)
			:col_style(th.spot.tbl_col)
			:cell_style(th.spot.tbl_cell)
			:widths { ui.Constraint.Length(14), ui.Constraint.Fill(1) }
	)
end

function M:spot_base(job)
	local info = ya.image_info(Url(job.file.path))
	if not info then
		return {}
	end

	return {
		ui.Row({ "Image" }):style(ui.Style():fg("green")),
		ui.Row { "  Format:", tostring(info.format) },
		ui.Row { "  Size:", string.format("%dx%d", info.w, info.h) },
		ui.Row { "  Color:", tostring(info.color) },
	}
end

return M
