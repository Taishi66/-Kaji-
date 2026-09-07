//! Complétion Tab de l'ARGUMENT d'une commande slash — la palette, elle, ne
//! complète que le NOM de la commande (`/wor` → `/workflow`). Dès qu'une
//! espace suit un nom connu, c'est l'argument attendu qui se complète :
//! `/workflow ` propose les recettes YAML du dossier de travail, `/theme` les
//! palettes, `/cost` ses vues.
//!
//! La table ci-dessous est la seule source : une commande absente n'attend pas
//! d'argument, et Tab y garde son comportement d'avant (palette, mention,
//! suggestion).

use std::path::Path;

/// Ce qu'une commande attend derrière son nom.
enum ArgKind {
    /// Fichiers du dossier de travail. Liste d'extensions vide = tout fichier.
    Files(&'static [&'static str]),
    /// Mots-clés fixes.
    Keywords(&'static [&'static str]),
    /// Noms de palettes, plus les deux mots-clés du sélecteur.
    Themes,
}

struct ArgSpec {
    command: &'static str,
    kind: ArgKind,
}

const ARG_COMPLETIONS: &[ArgSpec] = &[
    ArgSpec {
        command: "/workflow",
        kind: ArgKind::Files(&["yaml", "yml"]),
    },
    ArgSpec {
        command: "/edit",
        kind: ArgKind::Files(&[]),
    },
    ArgSpec {
        command: "/theme",
        kind: ArgKind::Themes,
    },
    ArgSpec {
        command: "/cost",
        kind: ArgKind::Keywords(&["models", "day", "week", "month", "cache", "projection"]),
    },
    ArgSpec {
        command: "/goal",
        kind: ArgKind::Keywords(&["clear"]),
    },
    ArgSpec {
        command: "/forge",
        kind: ArgKind::Keywords(&["full"]),
    },
    ArgSpec {
        command: "/editor",
        kind: ArgKind::Keywords(&["list", "reset", "mode"]),
    },
];

/// Entrées parcourues avant d'abandonner la marche. La complétion tourne sur
/// la boucle d'événements — contrairement à l'index des mentions, qui vit sur
/// sa propre tâche — donc elle se paie une marche bornée plutôt qu'un index
/// qui n'existe pas encore au premier Tab d'une session.
const MAX_SCAN: usize = 5_000;

/// Candidats retenus. Au-delà, la liste cesse d'être une liste.
const MAX_CANDIDATES: usize = 20;

/// L'argument sous le caret : où il commence (octets), ce qui en est déjà
/// tapé, et ce qui pourrait le compléter. `None` dès que la ligne ne nomme pas
/// une commande à argument — Tab garde alors son rôle d'avant.
pub(crate) struct ArgCandidates {
    pub start: usize,
    pub prefix: String,
    pub candidates: Vec<String>,
}

/// La ligne nomme une commande connue ET son argument a commencé. Test
/// purement syntaxique, sans marche de dossier : c'est lui qui décide à qui
/// appartient la touche Tab, avant de payer le calcul des candidats.
pub(crate) fn expects_argument(line: &str) -> bool {
    arg_span(line).is_some()
}

/// `line` est la ligne du composer JUSQU'AU caret : compléter regarde ce qui
/// est écrit devant, jamais la queue qu'on n'édite pas.
pub(crate) fn complete(line: &str, working_dir: &Path) -> Option<ArgCandidates> {
    let (spec, start) = arg_span(line)?;
    let prefix = line.get(start..).unwrap_or("");
    let candidates = match &spec.kind {
        ArgKind::Files(extensions) => files(working_dir, extensions, prefix),
        ArgKind::Keywords(words) => keywords(words.iter().copied(), prefix),
        ArgKind::Themes => keywords(
            crate::tui::theme::THEMES
                .iter()
                .map(|palette| palette.name)
                .chain(["list", "next"]),
            prefix,
        ),
    };
    Some(ArgCandidates {
        start,
        prefix: prefix.to_string(),
        candidates,
    })
}

/// La commande nommée en tête de ligne et l'octet où son argument commence.
/// Le nom doit être un mot entier suivi d'au moins une espace : `/workflow`
/// seul n'a pas encore d'argument (la palette le complète), et `/workflowx `
/// n'est pas `/workflow`.
fn arg_span(line: &str) -> Option<(&'static ArgSpec, usize)> {
    let space = line.find(' ')?;
    let name = line.get(..space)?;
    let spec = ARG_COMPLETIONS.iter().find(|spec| spec.command == name)?;
    let rest = line.get(space..)?;
    Some((spec, space + rest.len() - rest.trim_start().len()))
}

fn keywords<'a>(words: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<String> {
    let needle = prefix.to_lowercase();
    words
        .filter(|word| word.to_lowercase().starts_with(&needle))
        .map(str::to_string)
        .collect()
}

/// Marche bornée du dossier de travail, `.gitignore` et fichiers cachés
/// respectés comme l'index des mentions. Les dossiers ne sont pas des
/// arguments : `/workflow src/` ne se lance pas.
fn files(working_dir: &Path, extensions: &[&str], prefix: &str) -> Vec<String> {
    let needle = prefix.to_lowercase();
    let mut matches: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(working_dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    for (scanned, entry) in walker.flatten().enumerate() {
        if scanned >= MAX_SCAN {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(working_dir) else {
            continue;
        };
        let path = relative.to_string_lossy().replace('\\', "/");
        if !extensions.is_empty() {
            let extension = entry
                .path()
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase());
            if !extension.is_some_and(|ext| extensions.contains(&ext.as_str())) {
                continue;
            }
        }
        if !path.to_lowercase().starts_with(&needle) {
            continue;
        }
        matches.push(path);
    }
    matches.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    matches.truncate(MAX_CANDIDATES);
    matches
}

/// Le plus long préfixe que tous les candidats partagent — ce que Tab écrit
/// avant de proposer la liste. Comparé char par char : couper un `é` en deux
/// octets rendrait une chaîne invalide.
pub(crate) fn common_prefix(candidates: &[String]) -> String {
    let mut iter = candidates.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut common: Vec<char> = first.chars().collect();
    for candidate in iter {
        let shared = common
            .iter()
            .zip(candidate.chars())
            .take_while(|(a, b)| **a == *b)
            .count();
        common.truncate(shared);
    }
    common.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn demo_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("flow.yaml"), "name: flow\n").unwrap();
        std::fs::write(root.join("tache.yaml"), "name: tache\n").unwrap();
        std::fs::write(root.join("notes.md"), "salut\n").unwrap();
        (dir, root)
    }

    #[test]
    fn workflow_only_proposes_the_yaml_recipes() {
        let (_dir, root) = demo_dir();
        let found = complete("/workflow ", &root).unwrap();
        assert_eq!(found.candidates, vec!["flow.yaml", "tache.yaml"]);
        assert_eq!(found.prefix, "");
        assert_eq!(found.start, "/workflow ".len());
    }

    #[test]
    fn a_prefix_narrows_the_recipes_down_to_one() {
        let (_dir, root) = demo_dir();
        let found = complete("/workflow fl", &root).unwrap();
        assert_eq!(found.candidates, vec!["flow.yaml"]);
        assert_eq!(found.prefix, "fl");
    }

    #[test]
    fn edit_takes_any_file_where_workflow_takes_none_but_yaml() {
        let (_dir, root) = demo_dir();
        let edit = complete("/edit no", &root).unwrap();
        assert_eq!(edit.candidates, vec!["notes.md"]);
        let workflow = complete("/workflow no", &root).unwrap();
        assert!(workflow.candidates.is_empty());
    }

    #[test]
    fn a_command_without_an_expected_argument_completes_nothing() {
        let (_dir, root) = demo_dir();
        assert!(complete("/help ", &root).is_none());
        assert!(complete("bonjour ", &root).is_none());
    }

    /// Sans espace, l'argument n'a pas commencé : c'est encore le nom de la
    /// commande, et la palette garde Tab.
    #[test]
    fn a_bare_command_name_is_not_an_argument_yet() {
        let (_dir, root) = demo_dir();
        assert!(complete("/workflow", &root).is_none());
    }

    /// Même garde de mot entier que `slash_command_arg` : un futur
    /// `/workflowx` ne doit pas être lu comme `/workflow`.
    #[test]
    fn a_longer_command_name_is_not_the_one_in_the_table() {
        let (_dir, root) = demo_dir();
        assert!(complete("/workflowx fl", &root).is_none());
    }

    #[test]
    fn extra_spaces_push_the_argument_start_past_them() {
        let (_dir, root) = demo_dir();
        let found = complete("/workflow   fl", &root).unwrap();
        assert_eq!(found.prefix, "fl");
        assert_eq!(found.start, "/workflow   ".len());
    }

    #[test]
    fn keyword_commands_complete_their_vocabulary_case_insensitively() {
        let (_dir, root) = demo_dir();
        assert_eq!(complete("/cost da", &root).unwrap().candidates, vec!["day"]);
        assert_eq!(
            complete("/forge FU", &root).unwrap().candidates,
            vec!["full"]
        );
        assert_eq!(
            complete("/editor ", &root).unwrap().candidates,
            vec!["list", "reset", "mode"]
        );
    }

    #[test]
    fn theme_completes_the_palettes_and_the_selector_words() {
        let (_dir, root) = demo_dir();
        let found = complete("/theme ", &root).unwrap();
        for palette in crate::tui::theme::THEMES.iter() {
            assert!(
                found.candidates.iter().any(|c| c == palette.name),
                "{} absent de {:?}",
                palette.name,
                found.candidates
            );
        }
        assert!(found.candidates.iter().any(|c| c == "next"));
    }

    #[test]
    fn an_unknown_prefix_yields_no_candidate_rather_than_everything() {
        let (_dir, root) = demo_dir();
        assert!(complete("/cost zzz", &root).unwrap().candidates.is_empty());
        assert!(complete("/workflow zz", &root)
            .unwrap()
            .candidates
            .is_empty());
    }

    #[test]
    fn common_prefix_stops_where_the_candidates_diverge() {
        assert_eq!(common_prefix(&[]), "");
        assert_eq!(common_prefix(&["flow.yaml".to_string()]), "flow.yaml");
        assert_eq!(
            common_prefix(&["flow.yaml".to_string(), "flotte.yaml".to_string()]),
            "flo"
        );
        assert_eq!(
            common_prefix(&["flow.yaml".to_string(), "tache.yaml".to_string()]),
            ""
        );
    }

    /// Un préfixe commun coupé au milieu d'un char multi-octets rendrait une
    /// chaîne invalide : la comparaison se fait par char.
    #[test]
    fn common_prefix_never_cuts_a_char_in_half() {
        assert_eq!(
            common_prefix(&["modèles".to_string(), "modèle".to_string()]),
            "modèle"
        );
    }
}
