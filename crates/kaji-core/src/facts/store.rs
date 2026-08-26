use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use super::{Fact, FactType, validate_slug};

pub struct FactStore {
    dir: PathBuf,
}

impl FactStore {
    pub fn new(dir: PathBuf) -> Self {
        FactStore { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Vec<Fact> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                let file_name = path.file_name()?.to_str()?;
                if !file_name.ends_with(".md") || file_name == "MEMORY.md" {
                    return None;
                }
                let content = std::fs::read_to_string(&path).ok()?;
                Fact::parse(file_name, &content)
            })
            .collect()
    }

    pub fn get(&self, fact_type: &FactType, slug: &str) -> Option<Fact> {
        self.list()
            .into_iter()
            .find(|fact| fact.fact_type == *fact_type && fact.slug == slug)
    }

    pub fn write(&self, fact: &Fact) -> anyhow::Result<()> {
        if !validate_slug(&fact.slug) {
            anyhow::bail!("invalid slug: {}", fact.slug);
        }
        std::fs::create_dir_all(&self.dir)?;

        write_atomic(&self.dir, &fact.file_name(), fact.to_markdown().as_bytes())?;

        let mut facts = self.list();
        facts.retain(|existing| {
            !(existing.fact_type == fact.fact_type && existing.slug == fact.slug)
        });
        facts.push(fact.clone());
        write_atomic(
            &self.dir,
            "MEMORY.md",
            render_memory_index(&facts).as_bytes(),
        )?;

        Ok(())
    }
}

fn render_memory_index(facts: &[Fact]) -> String {
    let mut sorted: Vec<&Fact> = facts.iter().collect();
    sorted.sort_by(|a, b| {
        a.fact_type
            .as_str()
            .cmp(b.fact_type.as_str())
            .then_with(|| a.slug.cmp(&b.slug))
    });

    let mut out = String::from("# Memory Index\n\n> Généré par kaji — ne pas éditer.\n\n");
    for fact in sorted {
        let file_name = fact.file_name();
        out.push_str(&format!(
            "- [{file_name}]({file_name}) — {}\n",
            flatten_lines(&fact.description)
        ));
    }
    out
}

/// One fact per line in the generated index: a description carrying a line break
/// would otherwise forge extra entries, so the only renderer of stored text into
/// `MEMORY.md` flattens them.
fn flatten_lines(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn write_atomic(dir: &Path, file_name: &str, content: &[u8]) -> anyhow::Result<()> {
    let mut file = NamedTempFile::new_in(dir)?;
    file.write_all(content)?;
    file.persist(dir.join(file_name))?;
    Ok(())
}
