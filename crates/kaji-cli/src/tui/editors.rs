//! Catalogue d'éditeurs, détection sur le `PATH` et résolution du choix.
//!
//! Le contrat : `e` / `/edit` ne doit jamais finir en cul-de-sac. Ce qui est
//! installé est détecté, `/editor` en choisit un, et à défaut de tout choix la
//! résolution prend le premier éditeur terminal détecté plutôt que de parier
//! sur un `vi` qui peut ne pas être là.

use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKind {
    /// Prend le terminal : le TUI se suspend le temps de l'édition.
    Terminal,
    /// Ouvre sa propre fenêtre : lancé détaché, le TUI garde le terminal.
    Gui,
}

/// Comment l'éditeur veut qu'on lui demande une ligne.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineArg {
    /// `+42 fichier` — la convention vi.
    Plus,
    /// `--goto fichier:42` — VS Code et ses dérivés. Le drapeau est émis par
    /// [`Editor::argv`] plutôt que porté par les arguments du spec :
    /// `KAJI_EDITOR` est une commande libre (`code --wait`) qui hérite de la
    /// convention sans hériter des arguments, et `code --wait fichier:42`
    /// ouvrirait un fichier réellement nommé `fichier:42`.
    GotoColon,
    /// `fichier:42`.
    FileColon,
    /// `--line 42 fichier`.
    IdeaLine,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorSpec {
    pub id: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
    pub kind: EditorKind,
    pub line_arg: LineArg,
}

impl EditorSpec {
    /// La commande telle que `KAJI_EDITOR` la stocke.
    pub fn command(&self) -> String {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// L'ordre est celui du repli : la résolution sans choix explicite prend le
/// premier [`EditorKind::Terminal`] détecté, donc les éditeurs modaux d'abord.
pub const EDITORS: &[EditorSpec] = &[
    EditorSpec {
        id: "nvim",
        program: "nvim",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "vim",
        program: "vim",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "vi",
        program: "vi",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "hx",
        program: "hx",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::FileColon,
    },
    EditorSpec {
        id: "micro",
        program: "micro",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "nano",
        program: "nano",
        args: &[],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "emacs",
        program: "emacs",
        args: &["-nw"],
        kind: EditorKind::Terminal,
        line_arg: LineArg::Plus,
    },
    EditorSpec {
        id: "code",
        program: "code",
        args: &[],
        kind: EditorKind::Gui,
        line_arg: LineArg::GotoColon,
    },
    EditorSpec {
        id: "cursor",
        program: "cursor",
        args: &[],
        kind: EditorKind::Gui,
        line_arg: LineArg::GotoColon,
    },
    EditorSpec {
        id: "zed",
        program: "zed",
        args: &[],
        kind: EditorKind::Gui,
        line_arg: LineArg::FileColon,
    },
    EditorSpec {
        id: "subl",
        program: "subl",
        args: &[],
        kind: EditorKind::Gui,
        line_arg: LineArg::FileColon,
    },
    EditorSpec {
        id: "idea",
        program: "idea",
        args: &[],
        kind: EditorKind::Gui,
        line_arg: LineArg::IdeaLine,
    },
];

/// Un éditeur prêt à lancer : la commande, et ce qu'il faut savoir pour s'en
/// servir — qui du terminal ou de l'éditeur garde l'écran, et comment lui
/// demander une ligne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub program: String,
    pub args: Vec<String>,
    pub kind: EditorKind,
    pub line_arg: LineArg,
}

impl Editor {
    /// Tout ce qui suit le programme : ses propres arguments, la ligne dans la
    /// convention qu'il attend, puis le fichier.
    pub fn argv(&self, path: &Path, line: Option<usize>) -> Vec<String> {
        let mut argv = self.args.clone();
        let file = path.to_string_lossy().into_owned();
        match (line, self.line_arg) {
            (Some(line), LineArg::Plus) => {
                argv.push(format!("+{line}"));
                argv.push(file);
            }
            (Some(line), LineArg::GotoColon) => {
                argv.push("--goto".to_string());
                argv.push(format!("{file}:{line}"));
            }
            (Some(line), LineArg::FileColon) => argv.push(format!("{file}:{line}")),
            (Some(line), LineArg::IdeaLine) => {
                argv.push("--line".to_string());
                argv.push(line.to_string());
                argv.push(file);
            }
            _ => argv.push(file),
        }
        argv
    }

    /// Une ligne de commande libre (`KAJI_EDITOR`, `$VISUAL`, `$EDITOR`)
    /// découpée sur les blancs : aucun shell ne l'exécute, donc ni guillemets
    /// ni redirections. Le `kind` et la convention de ligne viennent du
    /// catalogue quand le programme y figure, sinon c'est un éditeur terminal
    /// en `+N` ; les arguments, eux, restent ceux de l'utilisateur.
    pub fn from_command(command: &str) -> Self {
        let mut words = command.split_whitespace();
        let program = words.next().unwrap_or_default().to_string();
        let args = words.map(str::to_string).collect();
        let spec = lookup(&program);
        Self {
            kind: spec.map_or(EditorKind::Terminal, |spec| spec.kind),
            line_arg: spec.map_or(LineArg::Plus, |spec| spec.line_arg),
            program,
            args,
        }
    }

    /// Le nom du programme sans son chemin : `KAJI_EDITOR=/opt/bin/nvim` doit
    /// se reconnaître dans le catalogue et s'afficher court.
    pub fn program_name(&self) -> &str {
        Path::new(&self.program)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(&self.program)
    }
}

impl From<&EditorSpec> for Editor {
    fn from(spec: &EditorSpec) -> Self {
        Self {
            program: spec.program.to_string(),
            args: spec.args.iter().map(|arg| (*arg).to_string()).collect(),
            kind: spec.kind,
            line_arg: spec.line_arg,
        }
    }
}

/// Les éditeurs du catalogue installés sur cette machine, dans l'ordre du
/// catalogue. Le `PATH` est injecté : la détection est pure et testable.
pub fn detect(path_var: &OsStr) -> Vec<&'static EditorSpec> {
    let dirs: Vec<_> = std::env::split_paths(path_var).collect();
    EDITORS
        .iter()
        .filter(|spec| {
            dirs.iter()
                .any(|dir| is_executable(&dir.join(spec.program)))
        })
        .collect()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// `KAJI_EDITOR` > `$VISUAL` > `$EDITOR` > premier terminal détecté. Les trois
/// premiers sont des lignes de commande libres, lues par
/// [`Editor::from_command`].
pub fn resolve(
    kaji_editor: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
    detected: &[&'static EditorSpec],
) -> Result<Editor, String> {
    if let Some(command) = [kaji_editor, visual, editor]
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
    {
        return Ok(Editor::from_command(command));
    }
    detected
        .iter()
        .find(|spec| spec.kind == EditorKind::Terminal)
        .map(|spec| Editor::from(*spec))
        .ok_or_else(no_editor_message)
}

fn lookup(program: &str) -> Option<&'static EditorSpec> {
    let name = Path::new(program).file_name()?.to_str()?;
    EDITORS.iter().find(|spec| spec.program == name)
}

/// Jamais de cul-de-sac : le message dit quoi faire et ce qui a été cherché.
pub fn no_editor_message() -> String {
    let programs: Vec<&str> = EDITORS.iter().map(|spec| spec.program).collect();
    format!(
        "aucun éditeur : définis $EDITOR ou /editor — cherché : {}",
        programs.join(", ")
    )
}

/// Ce que la session sait des éditeurs : ce qui est installé, ce que
/// l'environnement propose, et le choix explicite (`KAJI_EDITOR`, env ou
/// config). Lu une fois au démarrage — `/editor` ne fait que remplacer
/// `selected`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EditorState {
    pub detected: Vec<&'static EditorSpec>,
    pub selected: Option<String>,
    pub visual: Option<String>,
    pub editor: Option<String>,
}

impl EditorState {
    pub fn from_env(selected: Option<String>) -> Self {
        Self {
            detected: detect(&std::env::var_os("PATH").unwrap_or_default()),
            selected,
            visual: std::env::var("VISUAL").ok(),
            editor: std::env::var("EDITOR").ok(),
        }
    }

    pub fn resolve(&self) -> Result<Editor, String> {
        resolve(
            self.selected.as_deref(),
            self.visual.as_deref(),
            self.editor.as_deref(),
            &self.detected,
        )
    }

    /// `$VISUAL = nvim` tel que le sélecteur l'affiche, `None` si ni l'un ni
    /// l'autre n'est défini.
    pub fn env_label(&self) -> Option<String> {
        let (name, value) = [("$VISUAL", &self.visual), ("$EDITOR", &self.editor)]
            .into_iter()
            .find_map(|(name, value)| {
                value
                    .as_ref()
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| (name, value))
            })?;
        Some(format!("{name} = {value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec(id: &str) -> &'static EditorSpec {
        EDITORS
            .iter()
            .find(|spec| spec.id == id)
            .expect("éditeur au catalogue")
    }

    #[cfg(unix)]
    fn fake_path(programs: &[&str], plain: &[&str]) -> (tempfile::TempDir, std::ffi::OsString) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in programs {
            let path = dir.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for name in plain {
            std::fs::write(dir.path().join(name), "pas un exécutable").unwrap();
        }
        let path_var = std::env::join_paths([dir.path(), Path::new("/nulle/part")]).unwrap();
        (dir, path_var)
    }

    #[test]
    #[cfg(unix)]
    fn detect_keeps_the_executables_and_ignores_the_rest() {
        let (dir, path_var) = fake_path(&["nvim", "code"], &["vim"]);
        std::fs::create_dir(dir.path().join("hx")).unwrap();

        let found: Vec<&str> = detect(&path_var).iter().map(|spec| spec.id).collect();

        assert_eq!(
            found,
            vec!["nvim", "code"],
            "vim non exécutable, hx dossier"
        );
    }

    #[test]
    #[cfg(unix)]
    fn detect_on_an_empty_path_finds_nothing() {
        assert!(detect(OsStr::new("")).is_empty());
    }

    #[test]
    fn resolve_prefers_kaji_editor_then_visual_then_editor() {
        let detected = vec![spec("nvim")];
        assert_eq!(
            resolve(Some("hx"), Some("vim"), Some("nano"), &detected).unwrap(),
            Editor {
                program: "hx".to_string(),
                args: Vec::new(),
                kind: EditorKind::Terminal,
                line_arg: LineArg::FileColon,
            }
        );
        assert_eq!(
            resolve(None, Some("vim"), Some("nano"), &detected)
                .unwrap()
                .program,
            "vim"
        );
        assert_eq!(
            resolve(None, Some("   "), Some("nano"), &detected)
                .unwrap()
                .program,
            "nano"
        );
    }

    #[test]
    fn resolve_falls_back_to_the_first_terminal_editor_detected() {
        let detected = vec![spec("code"), spec("vim"), spec("nano")];
        let editor = resolve(None, None, None, &detected).unwrap();
        assert_eq!(editor.program, "vim", "un IDE graphique n'est pas un repli");
        assert_eq!(editor.kind, EditorKind::Terminal);
    }

    #[test]
    fn resolve_without_any_editor_says_what_it_looked_for() {
        let err = resolve(None, None, None, &[]).unwrap_err();
        assert!(err.contains("/editor"), "{err}");
        assert!(err.contains("nvim, vim, vi"), "{err}");
    }

    /// `--goto` appartient à la convention, pas au catalogue : une commande
    /// libre hérite de la première sans hériter du second, et `code --wait
    /// fichier:42` ouvrirait un fichier réellement nommé `fichier:42`.
    #[test]
    fn goto_colon_emits_its_own_flag_after_the_user_arguments() {
        let editor = Editor::from_command("code --wait");
        assert_eq!(
            editor.argv(&PathBuf::from("src/app.rs"), Some(42)),
            vec!["--wait", "--goto", "src/app.rs:42"]
        );
        assert_eq!(
            editor.argv(&PathBuf::from("src/app.rs"), None),
            vec!["--wait", "src/app.rs"]
        );
    }

    #[test]
    fn a_free_command_keeps_its_arguments_and_the_catalogue_traits() {
        let editor = resolve(Some("code --wait"), None, None, &[]).unwrap();
        assert_eq!(editor.args, vec!["--wait"]);
        assert_eq!(editor.kind, EditorKind::Gui);

        let absolute = resolve(Some("/opt/bin/nvim"), None, None, &[]).unwrap();
        assert_eq!(absolute.line_arg, LineArg::Plus);
        assert_eq!(absolute.program_name(), "nvim");
    }

    #[test]
    fn a_command_outside_the_catalogue_is_a_terminal_editor_in_plus() {
        let editor = resolve(Some("kak -e"), None, None, &[]).unwrap();
        assert_eq!(editor.kind, EditorKind::Terminal);
        assert_eq!(editor.line_arg, LineArg::Plus);
    }

    fn argv_of(id: &str, line: Option<usize>) -> Vec<String> {
        Editor::from(spec(id)).argv(&PathBuf::from("src/app.rs"), line)
    }

    #[test]
    fn each_editor_gets_the_line_argument_it_expects() {
        assert_eq!(argv_of("nvim", Some(42)), vec!["+42", "src/app.rs"]);
        assert_eq!(argv_of("hx", Some(42)), vec!["src/app.rs:42"]);
        assert_eq!(
            argv_of("code", Some(42)),
            vec!["--goto", "src/app.rs:42"],
            "code veut --goto devant fichier:ligne"
        );
        assert_eq!(
            argv_of("code", None),
            vec!["src/app.rs"],
            "pas de --goto nu"
        );
        assert_eq!(argv_of("zed", Some(42)), vec!["src/app.rs:42"]);
        assert_eq!(
            argv_of("idea", Some(42)),
            vec!["--line", "42", "src/app.rs"]
        );
        assert_eq!(argv_of("emacs", Some(42)), vec!["-nw", "+42", "src/app.rs"]);
    }

    #[test]
    fn without_a_line_only_the_file_is_passed() {
        assert_eq!(argv_of("nvim", None), vec!["src/app.rs"]);
        assert_eq!(argv_of("idea", None), vec!["src/app.rs"]);
        assert_eq!(argv_of("emacs", None), vec!["-nw", "src/app.rs"]);
    }

    #[test]
    fn an_editor_that_ignores_lines_never_gets_one() {
        let editor = Editor {
            program: "ed".to_string(),
            args: Vec::new(),
            kind: EditorKind::Terminal,
            line_arg: LineArg::None,
        };
        assert_eq!(
            editor.argv(&PathBuf::from("src/app.rs"), Some(42)),
            vec!["src/app.rs"]
        );
    }

    #[test]
    fn the_env_label_names_the_variable_that_wins() {
        let mut state = EditorState {
            visual: Some("nvim".to_string()),
            editor: Some("nano".to_string()),
            ..EditorState::default()
        };
        assert_eq!(state.env_label().as_deref(), Some("$VISUAL = nvim"));
        state.visual = Some("  ".to_string());
        assert_eq!(state.env_label().as_deref(), Some("$EDITOR = nano"));
        state.editor = None;
        assert_eq!(state.env_label(), None);
    }
}
