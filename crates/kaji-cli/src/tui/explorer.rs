//! Neo-tree-style file explorer (task 9) — the left column `Ctrl+E` opens.
//!
//! The tree is lazy: opening the pane reads the root directory and nothing
//! else, and every expansion reads exactly one directory
//! ([`ignore::WalkBuilder`] at `max_depth(1)`, `.gitignore` respected). What the
//! pane paints is the flattened [`ExplorerState::nodes`] — expanding splices a
//! directory's children in place, collapsing drains them back out — so the
//! renderer never walks a tree of its own.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Entries one directory ever contributes. Past this the listing is cut and a
/// single `… +N` row stands for the rest: a `node_modules` opened by accident
/// costs one bounded listing, not a hundred thousand rows to scroll through.
const MAX_DIR_ENTRIES: usize = 2_000;

/// Never listed, whatever `show_hidden` says — it is the repository's plumbing,
/// not a place anyone browses from a chat composer.
const GIT_DIR: &str = ".git";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Project-relative, `/`-separated, no trailing slash — see
    /// [`Node::mention_path`] for the form `a` attaches.
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// Set on the synthetic row standing for the entries a listing dropped at
    /// [`MAX_DIR_ENTRIES`]; carries how many were dropped.
    pub overflow: Option<usize>,
}

impl Node {
    /// What `a` puts in the composer: the mention index's own convention, where
    /// a directory carries a trailing `/`.
    pub fn mention_path(&self) -> String {
        if self.is_dir {
            format!("{}/", self.path)
        } else {
            self.path.clone()
        }
    }
}

#[derive(Debug)]
pub struct ExplorerState {
    pub root: PathBuf,
    /// The tree, flattened depth-first in display order.
    pub nodes: Vec<Node>,
    /// Index into `nodes` — always a row the current filter keeps.
    pub cursor: usize,
    pub show_hidden: bool,
    /// Incremental name filter (`/`), empty when off.
    pub filter: String,
    /// The filter line owns the keyboard: printable keys extend the query
    /// instead of navigating.
    pub filtering: bool,
}

impl ExplorerState {
    pub fn new(root: PathBuf) -> Self {
        let mut state = Self {
            root,
            nodes: Vec::new(),
            cursor: 0,
            show_hidden: false,
            filter: String::new(),
            filtering: false,
        };
        state.nodes = state.list_children("", 0);
        state
    }

    pub fn selected(&self) -> Option<&Node> {
        self.nodes.get(self.cursor)
    }

    /// Rows the filter keeps, in display order.
    pub fn visible(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| self.keeps(node))
            .map(|(i, _)| i)
            .collect()
    }

    /// Real entries on screen — the `… +N` row is a notice, not an element.
    pub fn entry_count(&self) -> usize {
        self.visible()
            .into_iter()
            .filter(|i| self.nodes[*i].overflow.is_none())
            .count()
    }

    fn keeps(&self, node: &Node) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        node.overflow.is_none()
            && node
                .name
                .to_lowercase()
                .contains(&self.filter.to_lowercase())
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let current = visible.iter().position(|i| *i == self.cursor).unwrap_or(0);
        let next = (current as isize + delta).clamp(0, visible.len() as isize - 1) as usize;
        self.cursor = visible[next];
    }

    pub fn cursor_to_start(&mut self) {
        if let Some(first) = self.visible().first() {
            self.cursor = *first;
        }
    }

    pub fn cursor_to_end(&mut self) {
        if let Some(last) = self.visible().last() {
            self.cursor = *last;
        }
    }

    /// `l`/`→`/`Enter` on a directory row.
    pub fn toggle_selected(&mut self) {
        let Some(node) = self.nodes.get(self.cursor) else {
            return;
        };
        if !node.is_dir {
            return;
        }
        if node.expanded {
            self.collapse(self.cursor);
        } else {
            self.expand(self.cursor);
        }
    }

    /// `h`/`←`: an open directory folds, anything else walks up one level.
    pub fn collapse_or_parent(&mut self) {
        let Some(node) = self.nodes.get(self.cursor) else {
            return;
        };
        if node.is_dir && node.expanded {
            self.collapse(self.cursor);
        } else {
            self.cursor_to_parent();
        }
    }

    fn cursor_to_parent(&mut self) {
        let Some(depth) = self.nodes.get(self.cursor).map(|node| node.depth) else {
            return;
        };
        if let Some(parent) = self.nodes[..self.cursor]
            .iter()
            .rposition(|node| node.depth < depth)
        {
            self.cursor = parent;
        }
    }

    fn expand(&mut self, index: usize) {
        let path = self.nodes[index].path.clone();
        let depth = self.nodes[index].depth;
        let children = self.list_children(&path, depth + 1);
        let added = children.len();
        self.nodes[index].expanded = true;
        self.nodes.splice(index + 1..index + 1, children);
        if self.cursor > index {
            self.cursor += added;
        }
    }

    fn collapse(&mut self, index: usize) {
        let depth = self.nodes[index].depth;
        let end = self.nodes[index + 1..]
            .iter()
            .position(|node| node.depth <= depth)
            .map(|offset| index + 1 + offset)
            .unwrap_or(self.nodes.len());
        let removed = end - (index + 1);
        self.nodes.drain(index + 1..end);
        self.nodes[index].expanded = false;
        if self.cursor >= end {
            self.cursor -= removed;
        } else if self.cursor > index {
            self.cursor = index;
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// `R`: re-reads every directory currently open, keeping the cursor on the
    /// same path when it survived.
    pub fn refresh(&mut self) {
        let expanded: HashSet<String> = self
            .nodes
            .iter()
            .filter(|node| node.is_dir && node.expanded)
            .map(|node| node.path.clone())
            .collect();
        let anchor = self
            .nodes
            .get(self.cursor)
            .filter(|node| node.overflow.is_none())
            .map(|node| node.path.clone());
        self.nodes = self.rebuild("", 0, &expanded);
        self.cursor = anchor
            .and_then(|path| {
                self.nodes
                    .iter()
                    .position(|node| node.overflow.is_none() && node.path == path)
            })
            .unwrap_or(0);
        self.snap_cursor();
    }

    fn rebuild(&self, rel: &str, depth: usize, expanded: &HashSet<String>) -> Vec<Node> {
        let mut out = Vec::new();
        for mut node in self.list_children(rel, depth) {
            if node.is_dir && expanded.contains(&node.path) {
                node.expanded = true;
                let children = self.rebuild(&node.path.clone(), depth + 1, expanded);
                out.push(node);
                out.extend(children);
            } else {
                out.push(node);
            }
        }
        out
    }

    pub fn start_filter(&mut self) {
        self.filtering = true;
    }

    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.snap_cursor();
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.snap_cursor();
    }

    /// `Esc` on the filter line: the query goes, the tree stays as it was.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.filtering = false;
        self.snap_cursor();
    }

    /// `Enter` on the filter line: the query stays applied, the keyboard goes
    /// back to navigating it.
    pub fn end_filter(&mut self) {
        self.filtering = false;
    }

    fn snap_cursor(&mut self) {
        let visible = self.visible();
        if !visible.is_empty() && !visible.contains(&self.cursor) {
            self.cursor = visible[0];
        }
    }

    /// One directory, one listing — `.gitignore` applied, `.git` dropped,
    /// directories first then names case-insensitively, cut at
    /// [`MAX_DIR_ENTRIES`].
    fn list_children(&self, rel: &str, depth: usize) -> Vec<Node> {
        let dir = if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        let mut entries = read_dir_sorted(&dir, self.show_hidden);
        let dropped = entries.len().saturating_sub(MAX_DIR_ENTRIES);
        entries.truncate(MAX_DIR_ENTRIES);
        let mut nodes: Vec<Node> = entries
            .into_iter()
            .map(|(name, is_dir)| Node {
                path: if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                },
                name,
                depth,
                is_dir,
                expanded: false,
                overflow: None,
            })
            .collect();
        if dropped > 0 {
            nodes.push(Node {
                path: String::new(),
                name: String::new(),
                depth,
                is_dir: false,
                expanded: false,
                overflow: Some(dropped),
            });
        }
        nodes
    }
}

fn read_dir_sorted(dir: &Path, show_hidden: bool) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(!show_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
        .flatten()
        .filter(|entry| entry.depth() > 0)
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.file_type().is_some_and(|t| t.is_dir()),
            )
        })
        .filter(|(name, _)| name != GIT_DIR)
        .collect();
    entries.sort_by(|(a_name, a_dir), (b_name, b_dir)| {
        b_dir
            .cmp(a_dir)
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
            .then_with(|| a_name.cmp(b_name))
    });
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/tui")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        std::fs::write(root.join("Cargo.toml"), "x").unwrap();
        std::fs::write(root.join("build.log"), "x").unwrap();
        std::fs::write(root.join(".env"), "x").unwrap();
        std::fs::write(root.join("src/main.rs"), "x").unwrap();
        std::fs::write(root.join("src/debug.log"), "x").unwrap();
        std::fs::write(root.join("src/tui/app.rs"), "x").unwrap();
        std::fs::write(root.join("target/debug"), "x").unwrap();
        dir
    }

    fn state() -> (ExplorerState, tempfile::TempDir) {
        let dir = fixture();
        (ExplorerState::new(dir.path().to_path_buf()), dir)
    }

    fn names(state: &ExplorerState) -> Vec<String> {
        state
            .visible()
            .into_iter()
            .map(|i| {
                let node = &state.nodes[i];
                match node.overflow {
                    Some(n) => format!("+{n}"),
                    None => node.name.clone(),
                }
            })
            .collect()
    }

    #[test]
    fn lists_the_root_directories_first_then_names_case_insensitively() {
        let (state, _dir) = state();
        assert_eq!(names(&state), vec!["src", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn gitignored_paths_never_show_up() {
        let (mut state, _dir) = state();
        assert!(!names(&state).contains(&"target".to_string()));
        assert!(!names(&state).contains(&"build.log".to_string()));

        state.toggle_selected();
        assert!(
            !names(&state).contains(&"debug.log".to_string()),
            "la règle du .gitignore racine vaut aussi dans un sous-dossier : {:?}",
            names(&state)
        );
    }

    #[test]
    fn the_git_directory_stays_hidden_even_with_dotfiles_on() {
        let (mut state, _dir) = state();
        state.toggle_hidden();
        assert!(state.show_hidden);
        assert!(
            names(&state).contains(&".env".to_string()),
            "{:?}",
            names(&state)
        );
        assert!(!names(&state).contains(&".git".to_string()));
    }

    #[test]
    fn dotfiles_are_hidden_until_the_toggle_and_come_back_off() {
        let (mut state, _dir) = state();
        assert!(!names(&state).contains(&".env".to_string()));
        state.toggle_hidden();
        assert!(names(&state).contains(&".env".to_string()));
        state.toggle_hidden();
        assert!(!names(&state).contains(&".env".to_string()));
    }

    #[test]
    fn the_listing_is_lazy_a_closed_directory_contributes_nothing() {
        let (state, _dir) = state();
        assert!(
            state.nodes.iter().all(|node| node.depth == 0),
            "{:?}",
            names(&state)
        );
        assert!(state.nodes.iter().all(|node| !node.expanded));
    }

    #[test]
    fn expanding_splices_the_children_and_collapsing_drains_them() {
        let (mut state, _dir) = state();
        state.toggle_selected();
        assert_eq!(
            names(&state),
            vec!["src", "tui", "main.rs", "Cargo.toml", "README.md"]
        );
        assert!(state.nodes[0].expanded);
        assert_eq!(state.nodes[1].path, "src/tui");
        assert_eq!(state.nodes[1].depth, 1);

        state.toggle_selected();
        assert_eq!(names(&state), vec!["src", "Cargo.toml", "README.md"]);
        assert!(!state.nodes[0].expanded);
    }

    #[test]
    fn expanding_a_nested_directory_keeps_the_flattened_order() {
        let (mut state, _dir) = state();
        state.toggle_selected();
        state.move_cursor(1);
        state.toggle_selected();
        assert_eq!(
            names(&state),
            vec!["src", "tui", "app.rs", "main.rs", "Cargo.toml", "README.md"]
        );
        assert_eq!(state.nodes[2].path, "src/tui/app.rs");
        assert_eq!(state.nodes[2].depth, 2);
    }

    #[test]
    fn h_folds_an_open_directory_then_walks_up_to_the_parent() {
        let (mut state, _dir) = state();
        state.toggle_selected();
        state.move_cursor(2);
        assert_eq!(state.selected().unwrap().name, "main.rs");

        state.collapse_or_parent();
        assert_eq!(state.selected().unwrap().name, "src", "remonte au parent");
        state.collapse_or_parent();
        assert!(!state.nodes[0].expanded, "replie le dossier ouvert");
        assert_eq!(state.selected().unwrap().name, "src");
    }

    #[test]
    fn a_directory_past_the_cap_is_cut_with_an_overflow_row() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_DIR_ENTRIES + 5 {
            std::fs::write(dir.path().join(format!("f{i:05}.txt")), "x").unwrap();
        }
        let state = ExplorerState::new(dir.path().to_path_buf());
        assert_eq!(state.nodes.len(), MAX_DIR_ENTRIES + 1);
        assert_eq!(state.nodes[MAX_DIR_ENTRIES].overflow, Some(5));
        assert_eq!(state.entry_count(), MAX_DIR_ENTRIES);
    }

    #[test]
    fn the_filter_keeps_matching_names_and_esc_empties_it() {
        let (mut state, _dir) = state();
        state.start_filter();
        for c in "read".chars() {
            state.push_filter(c);
        }
        assert_eq!(names(&state), vec!["README.md"]);
        assert_eq!(state.selected().unwrap().name, "README.md");

        state.pop_filter();
        assert_eq!(names(&state), vec!["README.md"]);
        state.clear_filter();
        assert!(!state.filtering);
        assert_eq!(names(&state), vec!["src", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_first_and_last_visible_row() {
        let (mut state, _dir) = state();
        state.cursor_to_end();
        assert_eq!(state.selected().unwrap().name, "README.md");
        state.cursor_to_start();
        assert_eq!(state.selected().unwrap().name, "src");
    }

    #[test]
    fn the_cursor_never_walks_off_either_end() {
        let (mut state, _dir) = state();
        state.move_cursor(-5);
        assert_eq!(state.cursor, 0);
        state.move_cursor(99);
        assert_eq!(state.selected().unwrap().name, "README.md");
    }

    #[test]
    fn refresh_rereads_open_directories_and_keeps_the_cursor_on_its_path() {
        let (mut state, dir) = state();
        state.toggle_selected();
        state.move_cursor(2);
        assert_eq!(state.selected().unwrap().path, "src/main.rs");

        std::fs::write(dir.path().join("src/added.rs"), "x").unwrap();
        state.refresh();
        assert_eq!(
            names(&state),
            vec![
                "src",
                "tui",
                "added.rs",
                "main.rs",
                "Cargo.toml",
                "README.md"
            ]
        );
        assert_eq!(
            state.selected().unwrap().path,
            "src/main.rs",
            "le curseur suit son chemin, pas son index"
        );
    }

    #[test]
    fn a_mention_path_marks_directories_with_a_trailing_slash() {
        let (state, _dir) = state();
        assert_eq!(state.nodes[0].mention_path(), "src/");
        assert_eq!(state.nodes[2].mention_path(), "README.md");
    }
}
