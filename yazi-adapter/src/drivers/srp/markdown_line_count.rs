//! Counts how many terminal rows a markdown document will occupy once
//! rendered — WITHOUT doing the actual styled layout (Som's own
//! `terminal_view::markdown_styling::layout_markdown` does that, on the
//! RECEIVING end; this driver's job is only to reserve a placeholder
//! footprint of the right size before Som has even parsed the file).
//! Hand-ported from Som's `crates/markdown_line_count` crate (kept in
//! this repo as a plain module rather than depending on that crate
//! directly — this whole driver is a hand-kept port of Som's `somcat`
//! client, not a shared-dependency relationship, see `srp/mod.rs`'s own
//! doc comment for why) — mirrors `count_rendered_lines`'s line-
//! producing logic exactly (block separators, hard breaks, code-block
//! line splits, list items, raw-HTML-block suppression) against
//! `pulldown_cmark` directly, so `markdown_show`'s placeholder grid
//! reserves the same row count Som's own widget will actually paint —
//! not the raw source file's newline count, which is systematically too
//! tall (markdown source formatting, e.g. blank lines between list
//! items, doesn't map 1:1 onto rendered rows — confirmed live as a real
//! bug: a gap of blank reserved rows between the widget and the next
//! prompt/preview boundary).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const PARSE_OPTIONS: Options = Options::ENABLE_TABLES
	.union(Options::ENABLE_FOOTNOTES)
	.union(Options::ENABLE_STRIKETHROUGH)
	.union(Options::ENABLE_TASKLISTS)
	.union(Options::ENABLE_SMART_PUNCTUATION)
	.union(Options::ENABLE_HEADING_ATTRIBUTES)
	.union(Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS)
	.union(Options::ENABLE_OLD_FOOTNOTES)
	.union(Options::ENABLE_GFM)
	.union(Options::ENABLE_SUPERSCRIPT)
	.union(Options::ENABLE_SUBSCRIPT);

/// Returns the number of rows Som's own `layout_markdown(source).len()`
/// would produce, without building any styled representation itself.
pub fn count_rendered_lines(source: &str) -> u32 {
	let mut lines: u32 = 0;
	let mut current_line_has_content = false;
	let mut list_depth: u32 = 0;
	let mut in_block_quote_depth: u32 = 0;
	let mut in_code_block = false;

	let finish_line = |lines: &mut u32, current_line_has_content: &mut bool| {
		*lines += 1;
		*current_line_has_content = false;
	};

	let insert_block_separator = |lines: &mut u32, list_depth: u32, in_block_quote_depth: u32| {
		if list_depth == 0 && in_block_quote_depth == 0 && *lines > 0 {
			*lines += 1;
		}
	};

	for event in Parser::new_ext(source, PARSE_OPTIONS) {
		match event {
			Event::Start(tag) => match tag {
				Tag::Heading { .. } => insert_block_separator(&mut lines, list_depth, in_block_quote_depth),
				Tag::List(_) => {
					insert_block_separator(&mut lines, list_depth, in_block_quote_depth);
					list_depth += 1;
				},
				Tag::CodeBlock(_) => {
					insert_block_separator(&mut lines, list_depth, in_block_quote_depth);
					in_code_block = true;
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					}
				},
				Tag::BlockQuote(_) => {
					insert_block_separator(&mut lines, list_depth, in_block_quote_depth);
					in_block_quote_depth += 1;
				},
				Tag::Paragraph => insert_block_separator(&mut lines, list_depth, in_block_quote_depth),
				_ => {},
			},
			Event::End(tag_end) => match tag_end {
				TagEnd::Heading(_) => {
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					} else {
						lines += 1;
					}
				},
				TagEnd::Paragraph => {
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					}
				},
				TagEnd::List(_) => list_depth = list_depth.saturating_sub(1),
				TagEnd::Item => {
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					}
				},
				TagEnd::CodeBlock => {
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					}
					in_code_block = false;
				},
				TagEnd::BlockQuote(_) => {
					if current_line_has_content {
						finish_line(&mut lines, &mut current_line_has_content);
					}
					in_block_quote_depth = in_block_quote_depth.saturating_sub(1);
				},
				_ => {},
			},
			Event::Text(text) => {
				if text.is_empty() {
					continue;
				}
				if in_code_block {
					let split_count = text.split('\n').count() as u32;
					lines += split_count.saturating_sub(1);
					current_line_has_content = !text.ends_with('\n');
					continue;
				}
				current_line_has_content = true;
			},
			Event::Code(text) | Event::InlineHtml(text) => {
				if !text.is_empty() {
					current_line_has_content = true;
				}
			},
			// `Event::Html` (a raw HTML BLOCK, as opposed to `InlineHtml`)
			// is deliberately ignored — Som's own `layout_markdown` never
			// matches `MarkdownTag::HtmlBlock`/`MarkdownTagEnd::HtmlBlock`/
			// `MarkdownEvent::Html` at all, so a raw `<table>...</table>`
			// block contributes zero rendered rows on the receiving end.
			// Counting it here (as `Code`/`InlineHtml` do) was the actual
			// root cause of a real, live-confirmed off-by-one in Som's own
			// `markdown_line_count` crate before this was ported.
			Event::Html(_) => {},
			Event::SoftBreak => current_line_has_content = true,
			Event::HardBreak => {
				if current_line_has_content {
					finish_line(&mut lines, &mut current_line_has_content);
				}
			},
			Event::Rule => {
				if current_line_has_content {
					finish_line(&mut lines, &mut current_line_has_content);
				}
				lines += 1;
			},
			Event::TaskListMarker(_) => current_line_has_content = true,
			_ => {},
		}
	}

	if current_line_has_content {
		lines += 1;
	}

	lines
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn plain_paragraph_is_one_line() { assert_eq!(count_rendered_lines("Hello world."), 1); }

	#[test]
	fn heading_is_one_line() { assert_eq!(count_rendered_lines("# Title"), 1); }

	#[test]
	fn two_paragraphs_get_a_blank_separator() {
		assert_eq!(count_rendered_lines("first\n\nsecond"), 3);
	}

	#[test]
	fn fenced_code_block_matches_source_line_count() {
		assert_eq!(count_rendered_lines("```\nline one\nline two\n```"), 2);
	}

	#[test]
	fn list_items_have_no_blank_separator_between_them() {
		assert_eq!(count_rendered_lines("- one\n- two"), 2);
	}

	#[test]
	fn horizontal_rule_is_one_line() {
		assert_eq!(count_rendered_lines("above\n\n---\n\nbelow"), 4);
	}

	#[test]
	fn raw_source_with_many_blank_lines_collapses() {
		let source = "first\n\n\n\n\nsecond\n\n\n\nthird";
		assert_eq!(count_rendered_lines(source), 5);
	}

	#[test]
	fn empty_source_is_zero_lines() { assert_eq!(count_rendered_lines(""), 0); }

	#[test]
	fn raw_html_block_contributes_zero_lines() {
		assert_eq!(count_rendered_lines("before\n\n<table>\n  <tr><td>x</td></tr>\n</table>\n\nafter"), 3);
	}

	#[test]
	fn matches_som_reference_on_the_real_markdown_md_fixture() {
		// Cross-checked directly against `layout_markdown(&source).len()`
		// on the same file in Som's own repo (326) — the two repos are
		// sibling checkouts on this dev machine (`C:\home\dnk\som`,
		// `C:\home\dnk\yazi-fork-work`), skips cleanly if that layout
		// doesn't hold (e.g. CI, a different machine).
		let Ok(source) = std::fs::read_to_string(r"C:\home\dnk\som\markdown.md") else {
			eprintln!("skipping: C:\\home\\dnk\\som\\markdown.md not present");
			return;
		};
		assert_eq!(count_rendered_lines(&source), 326);
	}
}
