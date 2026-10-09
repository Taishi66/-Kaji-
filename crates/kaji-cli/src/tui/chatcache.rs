use crate::tui::markdown::SourcePosition;
use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::mem::size_of;

const MAX_ENTRIES: usize = 256;
const MAX_RETAINED_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) struct AgentRender {
    pub lines: Vec<Line<'static>>,
    pub positions: Vec<Vec<SourcePosition>>,
    pub rows: usize,
}

struct Entry {
    source: String,
    rendered: AgentRender,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct ChatRenderCache {
    pub viewport: ChatViewport,
    entries: BTreeMap<usize, Entry>,
    retained_bytes: usize,
    width: u16,
    theme: usize,
    chat_len: usize,
    #[cfg(test)]
    disabled: bool,
    #[cfg(test)]
    hits: std::cell::Cell<usize>,
    #[cfg(test)]
    misses: std::cell::Cell<usize>,
}

#[derive(Default)]
pub(super) struct ChatViewport {
    size: (u16, u16),
    requested: Option<u16>,
    top: usize,
    anchor: Option<(usize, SourcePosition)>,
}

impl ChatViewport {
    pub fn user_scroll(&mut self) {
        self.requested = None;
    }

    pub fn requested_offset(&self) -> Option<u16> {
        self.requested
    }

    pub fn resolve(
        &mut self,
        rows: &[Vec<(usize, SourcePosition)>],
        width: u16,
        height: u16,
        requested: u16,
    ) -> usize {
        let overflow = rows.len().saturating_sub(usize::from(height));
        let mut top = overflow.saturating_sub(usize::from(requested));
        if requested > 0 && self.requested == Some(requested) {
            if self.size != (width, height) {
                if let Some((message, source)) = self.anchor {
                    if let Some(row) = rows.iter().position(|positions| {
                        positions.iter().any(|(id, position)| {
                            *id == message
                                && position.line == source.line
                                && position.cell == source.cell
                                && position.start <= source.start
                                && (source.start < position.end || position.start == position.end)
                        })
                    }) {
                        top = row.min(overflow);
                    }
                }
            } else {
                top = self.top.min(overflow);
            }
        }
        if requested == 0 || self.requested != Some(requested) || self.anchor.is_none() {
            self.anchor = rows.iter().skip(top).take(3).find_map(|positions| {
                positions
                    .iter()
                    .max_by_key(|(_, position)| position.end - position.start)
                    .copied()
            });
        }
        self.size = (width, height);
        self.requested = Some(requested);
        self.top = top;
        top
    }
}

impl ChatRenderCache {
    pub fn prepare(&mut self, width: u16, theme: usize, chat_len: usize) {
        if chat_len < self.chat_len {
            self.viewport = ChatViewport::default();
        }
        if self.width != width || self.theme != theme || chat_len < self.chat_len {
            self.clear_entries();
        }
        self.width = width;
        self.theme = theme;
        self.chat_len = chat_len;
    }

    pub fn clear(&mut self) {
        self.viewport = ChatViewport::default();
        self.clear_entries();
    }

    fn clear_entries(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    #[cfg(test)]
    pub fn set_disabled(&mut self, disabled: bool) {
        self.clear();
        self.disabled = disabled;
    }

    pub fn get(&self, index: usize, source: &str) -> Option<AgentRender> {
        let result = self
            .entries
            .get(&index)
            .filter(|entry| entry.source == source)
            .map(|entry| entry.rendered.clone());
        #[cfg(test)]
        if result.is_some() {
            self.hits.set(self.hits.get() + 1);
        } else {
            self.misses.set(self.misses.get() + 1);
        }
        result
    }

    #[cfg(test)]
    pub fn metrics(&self) -> (usize, usize, usize, usize) {
        (
            self.hits.get(),
            self.misses.get(),
            self.retained_bytes,
            self.entries.len(),
        )
    }

    #[cfg(test)]
    pub fn reset_metrics(&self) {
        self.hits.set(0);
        self.misses.set(0);
    }

    pub fn insert(&mut self, index: usize, source: &str, rendered: &AgentRender) {
        #[cfg(test)]
        if self.disabled {
            return;
        }
        if let Some(previous) = self.entries.remove(&index) {
            self.retained_bytes -= previous.bytes;
        }
        let bytes = clone_bytes(source, rendered);
        if bytes > MAX_RETAINED_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.retained_bytes + bytes > MAX_RETAINED_BYTES
        {
            let Some((&oldest, _)) = self.entries.first_key_value() else {
                break;
            };
            // Old history is visited before recent answers on every draw. It
            // must not evict those answers and turn every subsequent read into a miss.
            if oldest >= index {
                return;
            }
            if let Some((_, entry)) = self.entries.pop_first() {
                self.retained_bytes -= entry.bytes;
            }
        }
        let entry = Entry {
            source: source.to_string(),
            rendered: rendered.clone(),
            bytes,
        };
        self.retained_bytes += bytes;
        self.entries.insert(index, entry);
    }
}

// Clones own only their live elements, not the spare capacity of source buffers.
// BTreeMap node metadata is bounded separately by MAX_ENTRIES.
fn clone_bytes(source: &str, rendered: &AgentRender) -> usize {
    size_of::<Entry>()
        + source.len()
        + rendered.lines.len() * size_of::<Line<'static>>()
        + rendered
            .positions
            .iter()
            .map(|positions| {
                size_of::<Vec<SourcePosition>>() + positions.len() * size_of::<SourcePosition>()
            })
            .sum::<usize>()
        + rendered
            .lines
            .iter()
            .map(|line| {
                line.spans.len() * size_of::<Span<'static>>()
                    + line
                        .spans
                        .iter()
                        .map(|span| match &span.content {
                            Cow::Borrowed(_) => 0,
                            Cow::Owned(text) => text.len(),
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> AgentRender {
        AgentRender {
            lines: vec![Line::from(text.to_string())],
            positions: vec![Vec::new()],
            rows: 1,
        }
    }

    #[test]
    fn repeated_table_header_preserves_its_source_anchor_when_it_becomes_a_record_schema() {
        let text = include_str!("../../tests/fixtures/responsive-reading.md");
        let source = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.starts_with("| Capability |"))
            .nth(1)
            .unwrap()
            .0;
        let wide = crate::tui::markdown::render_markdown_layout(text, 80);
        let rows: Vec<_> = wide
            .positions
            .iter()
            .map(|positions| positions.iter().map(|position| (0, *position)).collect())
            .collect();
        let target = wide
            .positions
            .iter()
            .position(|positions| positions.iter().any(|position| position.line == source))
            .unwrap();
        let requested = (rows.len() - 12 - target) as u16;
        let mut viewport = ChatViewport::default();
        assert_eq!(viewport.resolve(&rows, 80, 12, requested), target);
        let anchor = viewport.anchor.unwrap();
        assert_eq!(anchor.1.line, source);
        for width in [40, 200, 40, 80] {
            let layout = crate::tui::markdown::render_markdown_layout(text, width);
            let rows: Vec<_> = layout
                .positions
                .iter()
                .map(|positions| positions.iter().map(|position| (0, *position)).collect())
                .collect();
            let top = viewport.resolve(&rows, width, 12, requested);
            assert!(rows[top].iter().any(|(_, position)| position.line == source
                && position.cell == anchor.1.cell
                && position.start <= anchor.1.start
                && anchor.1.start < position.end));
            assert_eq!(viewport.anchor.unwrap(), anchor);
        }
    }

    #[test]
    fn record_labels_and_header_only_records_keep_distinct_header_cells() {
        for suffix in ["", "\n| first value | second value | third value |"] {
            let text = format!("| First long heading | Second long heading | Third long heading |\n| - | - | - |{suffix}");
            let layout = crate::tui::markdown::render_markdown_layout(&text, 40);
            for cell in 0..3 {
                assert!(layout
                    .positions
                    .iter()
                    .flatten()
                    .any(|position| position.line == 0 && position.cell == cell));
            }
        }
    }

    #[test]
    fn recent_answers_survive_repeated_scans_of_more_history_than_the_cache_holds() {
        let mut cache = ChatRenderCache::default();
        cache.prepare(80, 0, MAX_ENTRIES * 2);
        for index in 0..MAX_ENTRIES * 2 {
            cache.insert(index, "answer", &block("answer"));
        }
        let mut hits = 0;
        for index in 0..MAX_ENTRIES * 2 {
            if cache.get(index, "answer").is_some() {
                hits += 1;
            } else {
                cache.insert(index, "answer", &block("answer"));
            }
        }
        assert_eq!(hits, MAX_ENTRIES);
        assert!(cache.get(MAX_ENTRIES * 2 - 1, "answer").is_some());
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn exact_source_width_theme_and_history_replacement_invalidate_the_result() {
        let mut cache = ChatRenderCache::default();
        cache.prepare(80, 0, 1);
        cache.insert(0, "before", &block("before"));
        assert!(cache.get(0, "before").is_some());
        assert!(cache.get(0, "after!").is_none());
        cache.insert(0, "after!", &block("after!"));
        assert!(cache.get(0, "before").is_none());

        cache.prepare(40, 0, 1);
        assert!(cache.get(0, "after!").is_none());
        cache.insert(0, "after!", &block("after!"));
        cache.prepare(40, 1, 1);
        assert!(cache.get(0, "after!").is_none());
        cache.insert(0, "after!", &block("after!"));
        cache.prepare(40, 1, 0);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.retained_bytes, 0);

        cache.prepare(40, 1, 10);
        cache.insert(0, "answer", &block("answer"));
        cache.prepare(40, 1, 5);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn oversized_answers_are_not_retained_and_byte_pressure_keeps_recent_ones() {
        let mut cache = ChatRenderCache::default();
        cache.prepare(80, 0, 40);
        let large = "x".repeat(MAX_RETAINED_BYTES);
        cache.insert(0, &large, &block(&large));
        assert!(cache.entries.is_empty());

        let text = "x".repeat(16 * 1024);
        for index in 1..40 {
            cache.insert(index, &text, &block(&text));
            assert!(cache.retained_bytes <= MAX_RETAINED_BYTES);
            assert!(cache.entries.len() <= MAX_ENTRIES);
        }
        assert!(cache.get(39, &text).is_some());
        let recent_bytes = cache.retained_bytes;
        cache.insert(0, &text, &block(&text));
        assert!(cache.get(39, &text).is_some());
        assert_eq!(cache.retained_bytes, recent_bytes);
    }
}
