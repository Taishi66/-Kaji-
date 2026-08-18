//! Catalogue d'éditeurs, détection sur le `PATH` et résolution du choix.
//!
//! Le contrat : `e` / `/edit` ne doit jamais finir en cul-de-sac. Ce qui est
//! installé est détecté, `/editor` en choisit un, et à défaut de tout choix la
//! résolution prend le premier éditeur terminal détecté plutôt que de parier
//! sur un `vi` qui peut ne pas être là.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// `KAJI_EDIT_MODE` (env > config, défaut `auto`) — ce que `/editor mode`
/// bascule et que [`plan`] consulte pour décider comment ouvrir un fichier
/// sans quitter kaji.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditMode {
    #[default]
    Auto,
    Suspend,
    Remote,
    Pane,
    Gui,
}

impl EditMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "suspend" => Some(Self::Suspend),
            "remote" => Some(Self::Remote),
            "pane" => Some(Self::Pane),
            "gui" => Some(Self::Gui),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Suspend => "suspend",
            Self::Remote => "remote",
            Self::Pane => "pane",
            Self::Gui => "gui",
        }
    }
}

/// Ce que kaji voit de son terminal hôte, posé une fois par `event_loop` à
/// partir de l'environnement et de `App::working_dir` — [`plan`] ne lit
/// jamais l'environnement lui-même, ce qui le garde pur et testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchContext {
    /// `$NVIM` — l'adresse du socket que le nvim hôte pose dans ses
    /// `:terminal`.
    pub nvim: Option<String>,
    pub zellij: bool,
    pub tmux: bool,
    pub cwd: PathBuf,
}

/// Ce que [`plan`] a décidé, argv complet (programme en tête) — `mod.rs` n'a
/// plus qu'à l'exécuter : suspendre le terminal, ou lancer sans suspension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Éditeur terminal prenant l'écran — le chemin T18 (`edit_file`).
    Suspend(Vec<String>),
    /// Éditeur graphique lancé sans suspension — le chemin 19a
    /// (`open_detached`).
    Detached(Vec<String>),
    /// `nvim --server $NVIM --remote-tab` — ouvre dans le nvim hôte.
    Remote(Vec<String>),
    /// Un pane du multiplexeur (Zellij ou tmux) — le premier mot de l'argv
    /// dit lequel.
    Pane(Vec<String>),
}

fn full_argv(editor: &Editor, file: &Path, line: Option<usize>) -> Vec<String> {
    std::iter::once(editor.program.clone())
        .chain(editor.argv(file, line))
        .collect()
}

/// Branche (a) : le nvim hôte peut recevoir l'onglet directement — seulement
/// si l'éditeur résolu EST ce nvim, sinon un `code` choisi comme
/// `KAJI_EDITOR` deviendrait un onglet du serveur juste parce que kaji tourne
/// dans son `:terminal`.
fn plan_remote(
    ctx: &LaunchContext,
    editor: &Editor,
    file: &Path,
    line: Option<usize>,
) -> Option<Launch> {
    let addr = ctx.nvim.as_ref()?;
    if !editor.program.ends_with("nvim") {
        return None;
    }
    let mut argv = vec![
        editor.program.clone(),
        "--server".to_string(),
        addr.clone(),
        "--remote-tab".to_string(),
    ];
    if let Some(line) = line {
        argv.push(format!("+{line}"));
    }
    argv.push(file.to_string_lossy().into_owned());
    Some(Launch::Remote(argv))
}

/// Branches (c)/(d) : Zellij d'abord — les deux variables peuvent coexister
/// (tmux lancé dans un pane Zellij), et Zellij est celui qui tourne au
/// premier plan.
fn plan_pane(
    ctx: &LaunchContext,
    editor: &Editor,
    file: &Path,
    line: Option<usize>,
) -> Option<Launch> {
    let cwd = ctx.cwd.to_string_lossy().into_owned();
    if ctx.zellij {
        let mut argv = vec![
            "zellij".to_string(),
            "run".to_string(),
            "-f".to_string(),
            "-c".to_string(),
            "--cwd".to_string(),
            cwd,
            "-n".to_string(),
            "kaji edit".to_string(),
            "--".to_string(),
        ];
        argv.extend(full_argv(editor, file, line));
        return Some(Launch::Pane(argv));
    }
    if ctx.tmux {
        let command = full_argv(editor, file, line)
            .iter()
            .map(|word| shell_quote(word))
            .collect::<Vec<_>>()
            .join(" ");
        return Some(Launch::Pane(vec![
            "tmux".to_string(),
            "split-window".to_string(),
            "-h".to_string(),
            "-c".to_string(),
            cwd,
            command,
        ]));
    }
    None
}

/// Un mot comme argument shell unique — `'` s'échappe en fermant le quoting,
/// une apostrophe littérale échappée, puis en le rouvrant.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `auto` essaie, dans l'ordre, le nvim hôte, l'éditeur graphique détaché,
/// puis le multiplexeur — le premier qui s'applique gagne, `Suspend` en
/// dernier repli. Les modes forcés retombent sur `Suspend` quand leur
/// condition n'est pas là plutôt que d'échouer : `mod.rs` compare `mode` au
/// résultat pour savoir s'il doit expliquer le repli.
pub fn plan(
    mode: EditMode,
    ctx: &LaunchContext,
    editor: &Editor,
    file: &Path,
    line: Option<usize>,
) -> Launch {
    match mode {
        EditMode::Auto => plan_remote(ctx, editor, file, line)
            .or_else(|| {
                (editor.kind == EditorKind::Gui)
                    .then(|| Launch::Detached(full_argv(editor, file, line)))
            })
            .or_else(|| plan_pane(ctx, editor, file, line))
            .unwrap_or_else(|| Launch::Suspend(full_argv(editor, file, line))),
        EditMode::Suspend => Launch::Suspend(full_argv(editor, file, line)),
        EditMode::Remote => plan_remote(ctx, editor, file, line)
            .unwrap_or_else(|| Launch::Suspend(full_argv(editor, file, line))),
        EditMode::Pane => plan_pane(ctx, editor, file, line)
            .unwrap_or_else(|| Launch::Suspend(full_argv(editor, file, line))),
        EditMode::Gui if editor.kind == EditorKind::Gui => {
            Launch::Detached(full_argv(editor, file, line))
        }
        EditMode::Gui => Launch::Suspend(full_argv(editor, file, line)),
    }
}

/// Étiquette humaine pour la ligne système de succès et pour `/editor mode`
/// — `Pane` lit son multiplexeur sur son propre argv (`zellij`/`tmux` en
/// tête, voir [`plan_pane`]) plutôt que de le redemander à `ctx`, pour ne
/// jamais dévier de ce qui a réellement tourné.
pub fn launch_label(launch: &Launch, editor: &Editor) -> String {
    match launch {
        Launch::Suspend(_) => "suspend".to_string(),
        Launch::Detached(_) => editor.program_name().to_string(),
        Launch::Remote(_) => "nvim hôte".to_string(),
        Launch::Pane(argv) => match argv.first().map(String::as_str) {
            Some("zellij") => "pane zellij".to_string(),
            _ => "pane tmux".to_string(),
        },
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

    fn ctx(nvim: Option<&str>, zellij: bool, tmux: bool) -> LaunchContext {
        LaunchContext {
            nvim: nvim.map(str::to_string),
            zellij,
            tmux,
            cwd: PathBuf::from("/work"),
        }
    }

    #[test]
    fn edit_mode_parse_is_case_insensitive_and_rejects_unknown_values() {
        assert_eq!(EditMode::parse("AUTO"), Some(EditMode::Auto));
        assert_eq!(EditMode::parse("Suspend"), Some(EditMode::Suspend));
        assert_eq!(EditMode::parse("remote"), Some(EditMode::Remote));
        assert_eq!(EditMode::parse("PANE"), Some(EditMode::Pane));
        assert_eq!(EditMode::parse(" gui "), Some(EditMode::Gui));
        assert_eq!(EditMode::parse("bogus"), None);
    }

    #[test]
    fn edit_mode_defaults_to_auto() {
        assert_eq!(EditMode::default(), EditMode::Auto);
    }

    #[test]
    fn auto_prefers_the_host_nvim_over_everything_else() {
        let editor = Editor::from(spec("nvim"));
        let launch = plan(
            EditMode::Auto,
            &ctx(Some("/tmp/nvim.sock"), true, true),
            &editor,
            &PathBuf::from("a.rs"),
            Some(3),
        );
        assert_eq!(
            launch,
            Launch::Remote(vec![
                "nvim".into(),
                "--server".into(),
                "/tmp/nvim.sock".into(),
                "--remote-tab".into(),
                "+3".into(),
                "a.rs".into(),
            ]),
            "nvim hôte gagne même avec Zellij et tmux tous les deux présents"
        );
    }

    #[test]
    fn auto_prefers_a_detached_gui_editor_before_the_multiplexer() {
        let editor = Editor::from(spec("code"));
        let launch = plan(
            EditMode::Auto,
            &ctx(None, true, true),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Detached(vec!["code".into(), "a.rs".into()]));
    }

    #[test]
    fn auto_uses_the_zellij_pane_when_no_nvim_host_or_gui_editor_applies() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Auto,
            &ctx(None, true, true),
            &editor,
            &PathBuf::from("a.rs"),
            Some(5),
        );
        assert_eq!(
            launch,
            Launch::Pane(vec![
                "zellij".into(),
                "run".into(),
                "-f".into(),
                "-c".into(),
                "--cwd".into(),
                "/work".into(),
                "-n".into(),
                "kaji edit".into(),
                "--".into(),
                "vim".into(),
                "+5".into(),
                "a.rs".into(),
            ]),
            "Zellij gagne sur tmux quand les deux sont là"
        );
    }

    #[test]
    fn auto_falls_back_to_tmux_when_zellij_is_absent() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Auto,
            &ctx(None, false, true),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(
            launch,
            Launch::Pane(vec![
                "tmux".into(),
                "split-window".into(),
                "-h".into(),
                "-c".into(),
                "/work".into(),
                "'vim' 'a.rs'".into(),
            ])
        );
    }

    #[test]
    fn auto_falls_back_to_suspend_with_no_host_context_at_all() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Auto,
            &LaunchContext::default(),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["vim".into(), "a.rs".into()]));
    }

    #[test]
    fn remote_mode_appends_the_line_flag_only_when_a_line_is_given() {
        let editor = Editor::from(spec("nvim"));
        let with_line = plan(
            EditMode::Remote,
            &ctx(Some("/tmp/s"), false, false),
            &editor,
            &PathBuf::from("a.rs"),
            Some(7),
        );
        assert_eq!(
            with_line,
            Launch::Remote(vec![
                "nvim".into(),
                "--server".into(),
                "/tmp/s".into(),
                "--remote-tab".into(),
                "+7".into(),
                "a.rs".into(),
            ])
        );
        let without_line = plan(
            EditMode::Remote,
            &ctx(Some("/tmp/s"), false, false),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(
            without_line,
            Launch::Remote(vec![
                "nvim".into(),
                "--server".into(),
                "/tmp/s".into(),
                "--remote-tab".into(),
                "a.rs".into(),
            ]),
            "pas de +N nu sans ligne"
        );
    }

    #[test]
    fn remote_mode_without_nvim_host_falls_back_to_suspend() {
        let editor = Editor::from(spec("nvim"));
        let launch = plan(
            EditMode::Remote,
            &LaunchContext::default(),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["nvim".into(), "a.rs".into()]));
    }

    #[test]
    fn remote_mode_with_a_non_nvim_editor_falls_back_to_suspend() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Remote,
            &ctx(Some("/tmp/nvim.sock"), false, false),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["vim".into(), "a.rs".into()]));
    }

    #[test]
    fn pane_mode_without_a_multiplexer_falls_back_to_suspend() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Pane,
            &LaunchContext::default(),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["vim".into(), "a.rs".into()]));
    }

    #[test]
    fn gui_mode_on_a_terminal_editor_falls_back_to_suspend() {
        let editor = Editor::from(spec("vim"));
        let launch = plan(
            EditMode::Gui,
            &LaunchContext::default(),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["vim".into(), "a.rs".into()]));
    }

    #[test]
    fn gui_mode_on_a_gui_editor_detaches() {
        let editor = Editor::from(spec("code"));
        let launch = plan(
            EditMode::Gui,
            &LaunchContext::default(),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Detached(vec!["code".into(), "a.rs".into()]));
    }

    #[test]
    fn suspend_mode_always_suspends_even_with_a_full_host_context() {
        let editor = Editor::from(spec("nvim"));
        let launch = plan(
            EditMode::Suspend,
            &ctx(Some("/tmp/nvim.sock"), true, true),
            &editor,
            &PathBuf::from("a.rs"),
            None,
        );
        assert_eq!(launch, Launch::Suspend(vec!["nvim".into(), "a.rs".into()]));
    }

    /// Un chemin avec un espace et une apostrophe : chaque mot est entre
    /// apostrophes, et l'apostrophe littérale ferme/rouvre le quoting au
    /// lieu de casser la commande shell.
    #[test]
    fn tmux_pane_quotes_each_argv_word_posix_style() {
        let editor = Editor::from(spec("vim"));
        let file = PathBuf::from("my file's.rs");
        let launch = plan(
            EditMode::Pane,
            &ctx(None, false, true),
            &editor,
            &file,
            Some(2),
        );
        let Launch::Pane(argv) = launch else {
            panic!("pane attendu");
        };
        assert_eq!(
            argv,
            vec![
                "tmux".to_string(),
                "split-window".to_string(),
                "-h".to_string(),
                "-c".to_string(),
                "/work".to_string(),
                r"'vim' '+2' 'my file'\''s.rs'".to_string(),
            ]
        );
    }

    #[test]
    fn launch_label_names_the_multiplexer_and_the_gui_program() {
        let vim = Editor::from(spec("vim"));
        let code = Editor::from(spec("code"));
        assert_eq!(launch_label(&Launch::Suspend(vec![]), &vim), "suspend");
        assert_eq!(launch_label(&Launch::Remote(vec![]), &vim), "nvim hôte");
        assert_eq!(launch_label(&Launch::Detached(vec![]), &code), "code");
        assert_eq!(
            launch_label(&Launch::Pane(vec!["zellij".into()]), &vim),
            "pane zellij"
        );
        assert_eq!(
            launch_label(&Launch::Pane(vec!["tmux".into()]), &vim),
            "pane tmux"
        );
    }
}
