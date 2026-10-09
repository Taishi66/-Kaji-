use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::mem::size_of;

const MAX_ENTRIES: usize = 32;
const MAX_RETAINED_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub(super) struct AgentRender {
    pub lines: Vec<Line<'static>>,
    pub rows: usize,
    pub last_rows: usize,
}

struct Entry {
    source: String,
    rendered: AgentRender,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct ChatRenderCache {
    entries: BTreeMap<usize, Entry>,
    retained_bytes: usize,
    width: u16,
    theme: usize,
    chat_len: usize,
    #[cfg(test)]
    disabled: bool,
}

impl ChatRenderCache {
    pub fn prepare(&mut self, width: u16, theme: usize, chat_len: usize) {
        if self.width != width || self.theme != theme || chat_len < self.chat_len {
            self.clear();
        }
        self.width = width;
        self.theme = theme;
        self.chat_len = chat_len;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    #[cfg(test)]
    pub fn set_disabled(&mut self, disabled: bool) {
        self.clear();
        self.disabled = disabled;
    }

    pub fn get(&self, index: usize, source: &str) -> Option<AgentRender> {
        self.entries
            .get(&index)
            .filter(|entry| entry.source == source)
            .map(|entry| entry.rendered.clone())
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
            rows: 1,
            last_rows: 1,
        }
    }

    #[test]
    fn recent_answers_survive_repeated_scans_of_more_history_than_the_cache_holds() {
        let mut cache = ChatRenderCache::default();
        cache.prepare(80, 0, 64);
        for index in 0..64 {
            cache.insert(index, "answer", &block("answer"));
        }
        let mut hits = 0;
        for index in 0..64 {
            if cache.get(index, "answer").is_some() {
                hits += 1;
            } else {
                cache.insert(index, "answer", &block("answer"));
            }
        }
        assert_eq!(hits, 32);
        assert!(cache.get(63, "answer").is_some());
        assert_eq!(cache.entries.len(), 32);
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
