# Responsive terminal reading

## Problem

The transcript limits all content to 102 columns and tables to 100, even when the chat pane has 200 columns. Table cells are shortened with an ellipsis, permanently hiding their endings. Full-height frames and underlined gold headings dominate the reading hierarchy. Resizing keeps a distance from the bottom rather than the passage being read.

## Direction reviewed for implementation

The transcript owns the available chat pane width, with small adaptive horizontal margins. Ordinary prose wraps at a comfortable reading measure of approximately 88 columns; tables use the available width with individual prose cells measured at up to 96 columns, and code uses the available width. The agent identity occupies a separate, quiet role line so its prefix cannot shift table geometry. Product labels remain English, with kanji as restrained brand accents.

Tables retain all cell content and inline styling. On suitable widths they use multiline cells, useful minimum column measures, column gutters with a subtle header separator and no vertical rails, and enough spacing to distinguish multiline records. When columns would become unreadable, rows become stacked records. Two-column tables preserve their header schema once, then use the first value as a small bold record title and the second as the body. Wider schemas keep each label. Terminal-cell widths, Unicode graphemes, and long unbroken values must not cause missing text or broken boundaries. Malformed tables retain their existing plain-text fallback.

The transcript loses its enclosing rectangle. A quiet chat/status line and a compact composer separator provide structure. The single-line composer retains its horizontal caret scrolling and uses three or four terminal rows; it does not grow with text. User queries use neutral body text and a distinct colored role without a selection-like background. The agent role explicitly names kaji next to its kanji. Markdown headings use bold text without pervasive gold or underlines. Existing themes remain selectable; this is a layout and hierarchy change.

Resize preserves the passage being read using a logical message/block/text-position anchor where practical. A user following the bottom continues following the bottom. Width changes recompute actual rendered heights and turn positions. Cache keys retain width, theme, and source identity. The existing bounded rendering cache is reused, with source positions included in its retained-byte accounting. Its limits increase from 32 answers / 128 KiB to 256 answers / 1 MiB after the bounded debug benchmark showed systematic reparsing of a 128-answer history at the smaller limits. This adds 896 KiB to the explicit retained payload bound; oversize answers still are not retained. Virtualization and unbounded parsed histories remain outside scope.

## Scope and isolation

Changes concern terminal rendering, Markdown layout, cache measurements, and scroll anchoring only. Agent behavior, providers, replay state, and security changes are independent. Existing uncommitted TUI work is preserved; its baseline is copied under `/tmp/kaji-ux-baseline-2026-10-09`. The implementation agent owns rendering code; the publication agent owns the final repository documentation and roadmaps, preserving unrelated local work.

## Acceptance and verification

The user rejected the visual readability of the previous functional rendering (clipboard screenshot dated 2026-10-09 19:46:07). Earlier passing functional checks remain evidence of those checks only. Visual and responsive acceptance is reopened; this design direction is reviewed for implementation, not accepted by the user as a finished interface. Acceptance remains pending a review of the new actual terminal renders. Existing progress percentages do not establish visual acceptance.


Use a fixture reproducing the reported screenshot: two tables with long labels and explanatory cells, several headings, and prose. Render at 40, 80, 120, and 200 columns. Check complete final-cell sentinels, inline styles, CJK/grapheme widths, bounded physical lines, real wrapped heights, and cached/fresh rendering equality.

Exercise 80/120/200 to 40 to 200 resize while scrolled into a known passage, confirming its sentinel remains in view. Check follow-bottom separately. Capture real terminal output with tmux/ANSI and a reviewable buffer image where feasible. Measure repeated frame rendering on a realistic history to catch additional reparsing or height-measurement regressions.

Run targeted CLI tests, required clippy, and a distinct validation build after the ongoing security validation releases the single Cargo slot. Preserve the binary currently being tested by the user.
